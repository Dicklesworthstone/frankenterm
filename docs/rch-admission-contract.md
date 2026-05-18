# RCH Admission Diagnostic Contract

`ft doctor rch-admission` is the target operator surface for a read-only
preflight that explains whether a FrankenTerm proof lane can safely claim RCH
remote Cargo evidence. This contract is static in `ft-69gwh.1`; it defines the
JSON shape, stable reason-code vocabulary, fixture coverage, and forbidden
operator actions before the live collector is wired.

The output is advisory. It can say a command appears runnable or blocked, but it
is never proof that Cargo, tests, clippy, benches, fuzzers, or release gates
passed. A dry-run, queue snapshot, worker-health report, or schema-valid
diagnostic must not be cited as compiled proof.

## Schema

The schema lives at `docs/json-schema/ft-rch-admission.json` and uses
`contract_id = "ft.rch_admission.v1"`. Required top-level fields are:

| Field | Purpose |
|---|---|
| `command` | Raw command, normalized command, classification, intercept decision, and target dir. |
| `local_disk` | `/System/Volumes/Data`, `/private/tmp`, repo write probe, and RCH cache write probe. |
| `beads` | Beads DB/JSONL writeability and active/blocking bead context. |
| `agent_mail` | Agent Mail API/DB state plus reservation conflicts. |
| `rch_queue` | Queue posture, active project exclusion, active/queued builds, and worker counts. |
| `worker_rejections` | Per-worker or aggregate rejection rows. |
| `cargo_jobs` | Parsed explicit Cargo job count when available. |
| `estimated_slots` | Final RCH slot estimate when available. |
| `reason_codes` | Stable blocker or advisory reason codes. |
| `recommendations` | Safe next actions tied back to reason codes. |
| `forbidden_actions` | Actions that must not be taken by normal agents. |

## Reason Codes

| Code | Meaning | Safe next action |
|---|---|---|
| `local_eno_space` | Local disk or cache write probes failed with ENOSPC or equivalent. | Stop proof/patch work, retain evidence, ask for operator-approved cleanup if needed. |
| `no_admissible_workers` | RCH selected no worker for the command. | Inspect worker rejection details and block the proof bead rather than falling back locally. |
| `critical_pressure` | Workers were rejected for root, project, or cache pressure. | Wait for worker recovery or operator-approved cleanup on the worker side. |
| `telemetry_gap` | Worker health or capability data is stale or missing. | Refresh read-only status/probe data; do not mutate workers. |
| `insufficient_slots` | Healthy workers lacked enough free slots for the estimated job. | Reduce explicit Cargo jobs when appropriate, queue, or wait. |
| `active_project_exclusion` | Same-project RCH active build excludes another proof lane. | Wait for that build or use Beads/Agent Mail to coordinate ownership. |
| `speedscore_response_shape` | SpeedScore or related RCH API response shape failed parsing. | Treat ranking data as unavailable and rely on stable worker/status evidence. |
| `dry_run_inconsistent_worker` | Dry-run envelope claims worker availability while selected worker is null or skipped. | Preserve both fields and classify the result as advisory/inconclusive. |
| `unknown` | The blocker is real but does not fit a stable code yet. | File or update a bead with the retained artifact and propose a new code. |

## Forbidden Actions

The contract always carries `forbidden_actions`. Normal agents must not perform
any of these stable action identifiers:

| Action | Meaning |
|---|---|
| `run_local_cargo_as_proof` | Do not cite local Cargo as proof for a FrankenTerm Cargo lane. |
| `restart_agent_mail` | Do not restart the shared Agent Mail service. |
| `repair_agent_mail_db` | Do not run Agent Mail repair/reconstruct commands. |
| `restart_rch_daemon` | Do not restart the RCH daemon while collecting admission evidence. |
| `mutate_rch_worker` | Do not enable, disable, clean, or otherwise mutate RCH workers. |
| `cancel_other_agent_build` | Do not cancel another agent's active build to make room. |
| `delete_files_without_approval` | Do not delete files without explicit written approval. |
| `treat_dry_run_as_compile_proof` | Do not treat a dry-run or schema-valid diagnostic as compile/test proof. |

## Fixtures

Reduced fixtures live in `fixtures/rch-admission/reason-code-fixtures.json`.
They cover every reason-code family above using minimal retained command-output
shapes. The static verifier
`tests/e2e/test_rch_admission_contract.sh` checks that the schema enum, docs,
fixtures, provenance row, and README E2E count remain synchronized.

## Read-only Collector Substrate

`crates/frankenterm-core/src/rch_admission.rs` provides the pure normalization
layer for follow-on live wiring. It accepts already-collected observations for
local disk, `/private/tmp`, Beads, Agent Mail, RCH queue state, worker
rejections, Cargo job estimates, and dirty git paths, then emits the existing
`ft.rch_admission.v1` report shape. Collector observations retain source
command/API, freshness, and error-category metadata in citations so later
doctor wiring can explain where evidence came from without expanding the stable
schema.

The core module intentionally does not shell out, write Beads state, probe Agent
Mail databases, restart services, mutate RCH workers, cancel builds, run Cargo,
or delete files. CLI and doctor collectors must perform their read-only probes
outside this module and pass redacted facts into the normalizer.

## Cargo Command Analyzer

`analyze_rch_admission_cargo_command` is the pure parser used by follow-on
doctor wiring to explain Cargo-shaped proof commands before an agent attempts a
material RCH run. It tokenizes an already-known command string and caller
provided environment facts; it does not execute the command, query RCH, inspect
workers, or mutate local state.

The analyzer feeds the existing v1 fields rather than adding a second schema
surface:

| Output field | Analyzer source |
|---|---|
| `command.normalized` | The `cargo ...` suffix after wrappers such as `rch exec --` or `env`. |
| `command.classification` | Cargo subcommand family such as `cargo_test`, `cargo_check`, `cargo_clippy`, or `cargo_build`. |
| `command.target_dir` | `CARGO_TARGET_DIR`, `--target-dir VALUE`, or `--target-dir=VALUE`. |
| `cargo_jobs` | Explicit `cargo -j`, `cargo --jobs`, or `CARGO_BUILD_JOBS` value when present. |
| `estimated_slots` | The explicit job count when present; otherwise the installed selector estimate when supplied, falling back to one advisory slot. |
| `citations` | A summary explaining explicit versus inferred job count, package scope, test scope, target dir, installed selector estimate, and whether the selector estimate mismatched the explicit command. |

Job-source precedence is intentional: Cargo `-j` / `--jobs` wins over
`CARGO_BUILD_JOBS`; `CARGO_BUILD_JOBS` wins over an installed RCH selector
estimate; and the final fallback is a one-slot advisory estimate. When an
explicit job count differs from the installed selector estimate, the analyzer
sets `slot_estimate_mismatch=true` in the citation summary. That mismatch is
evidence for humans and follow-up beads; it is not by itself compiled proof and
must not be cited as a Cargo result.
