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
