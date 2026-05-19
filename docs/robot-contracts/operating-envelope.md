# Robot Family Contract: `operating-envelope`

**Bead:** `ft-booek.1`
**Status:** planning contract only. No runtime controller implementation is
shipped by this document.

## Purpose

The swarm operating envelope composes current coordination, proof, and capacity
signals into a dry-run admission window before agents are started, Beads are
claimed, files are edited, or proof lanes are consumed. The output contract is
`ft.operating_envelope.v1`, defined in
`docs/json-schema/ft-operating-envelope.json`.

The contract is deliberately read-only and fail-closed. Missing, stale,
contradictory, privacy-redacted, or unproven signals lower the envelope and emit
typed reason codes instead of silently inferring that work is safe.

## Required Domains

Every valid envelope records source snapshots for:

- capacity/resource posture;
- RCH status, worker selection, topology preflight, and whether Cargo was
  reached remotely;
- Beads ready, blocked, in-progress, stale, assignee, and overlap state;
- Agent Mail health or an unavailable/degraded fallback state;
- git tracked/untracked dirty-path and deletion-risk state; and
- optional robot inventory metadata without raw pane text.

Source snapshots record provenance, freshness, redaction posture, reason codes,
and retained artifact paths. Agent Mail and RCH failures are input facts, not
repair instructions.

## Decision Semantics

`decision.outcome` uses a small action vocabulary:

| Outcome | Meaning |
| --- | --- |
| `admit` | The requested window can proceed within bounded concurrency. |
| `defer` | Work remains possible later, but proof or capacity is not currently trustworthy. |
| `degrade` | Only a reduced window is safe, usually read-only or static-check-only. |
| `shed` | Current pressure requires shedding or refusing new work. |
| `block` | A hard contradiction or policy gate prevents the requested window. |
| `wait` | Another owner, dirty overlap, or external queue must clear first. |

`admission_windows[*]` lists the action classes currently allowed and forbidden.
`local_cargo_proof`, `raw_pane_content`, `raw_pane_content_capture`,
`pane_mutation`, `service_mutation`, `service_restart`, `agent_mail_repair`,
`rch_daemon_restart`, `worker_drain`, `build_cancellation`,
`destructive_filesystem`, and `destructive_git` are forbidden by the v1
side-effect policy.

## Required Reason-Code Families

Implementations should preserve these families:

- `capacity.*` and `target_hardware.*` for pressure and target-class proof;
- `rch.*` for worker selection, topology preflight, active-project exclusion,
  and remote Cargo reachability;
- `beads.*` and `assignee_overlap.*` for queue and owner state;
- `agent_mail.*` for health and fallback posture;
- `git.*`, `dirty_overlap.*`, and `deletion_risk.*` for shared-tree risk;
- `robot.*` for inventory-only pane state; and
- `fail_closed.*`, `policy.*`, and `source.*` for envelope reductions.

## Fixtures

Fixtures live under `fixtures/operating-envelope/`:

- `valid/healthy.json`
- `valid/agent-mail-unavailable.json`
- `valid/rch-no-worker.json`
- `valid/rch-topology-failure.json`
- `valid/dirty-overlap.json`
- `valid/target-hardware-skipped.json`
- `invalid/missing-field.json`
- `invalid/missing-contract-id.json`
- `invalid/unknown-version.json`
- `invalid/malformed-path.json`

The negative fixtures are intentionally parseable JSON that must fail the schema
or the equivalent static contract checks.

## Proof-Calendar Fixture Contract

`ft-booek.8` adds a nested read-only proof-calendar fixture contract for RCH
outages and target-class proof gaps. The schema is
`docs/json-schema/ft-operating-envelope-proof-calendar.json`; the retained
golden corpus is
`fixtures/operating-envelope/proof-calendar/cases.v1.json`.

The proof-calendar contract id is
`ft.operating_envelope.proof_calendar.v1`. It classifies candidate work into:

- `static_docs_fixture_verifier`
- `shell_jq_contract`
- `rch_required_unit_integration`
- `target_class_hardware_proof`
- `operator_only_recovery`
- `forbidden_mutation`

The golden cases are:

- `rch-unavailable`
- `no-admissible-workers`
- `static-only-ready`
- `local-closed-not-published`
- `dirty-overlap`
- `stale-proof-artifact`
- `target-hardware-unavailable`

The negative proof-calendar corpus is
`fixtures/operating-envelope/proof-calendar/invalid/cases.v1.json`. It retains
parseable JSON fragments that the static verifier must reject as unsafe:

- `local-cargo-fallback-allowed`
- `raw-pane-content-allowed`
- `absolute-artifact-path`
- `missing-required-forbidden-action`
- `toon-row-width-mismatch`
- `service-mutation-permitted`

Every case emits deterministic `now`, `next`, and `wait` lanes with stable
reason codes, source snapshots for Beads/RCH/Agent Mail/git/proof-artifact
freshness, a TOON-ready row projection, and the same fail-closed proof policy:

- `rch_sync_chatter_counts_as_remote_proof: false`
- `dry_run_interception_counts_as_remote_proof: false`
- `local_shell_success_counts_as_remote_cargo_proof: false`
- `local_cargo_fallback_allowed: false`
- `remote_cargo_proof_requires_retained_artifact: true`

The no-service-action guarantee is explicit. Proof-calendar artifacts must
forbid `agent_mail_service_repair`, `build_cancellation`, `delete_files`,
`destructive_filesystem`, `destructive_git`, `local_cargo_proof`,
`local_heavy_cargo_fallback`, `rch_daemon_restart`, `rch_service_repair`,
`service_mutation`, and `worker_mutation` at both the artifact root and every
calendar entry.

The static verifier is `bash tests/e2e/test_operating_envelope_fixture_manifest.sh`.
Use `--json` for machine-readable summary output that includes the base fixture
counts, proof-calendar case count, and proof-calendar invalid case count. The
fixture manifest retains this verifier command in `static_checks` so manifest
consumers can discover the full static proof lane from the artifact itself.
