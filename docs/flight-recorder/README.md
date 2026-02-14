# Flight Recorder Docs Index

This directory contains design contracts, validation artifacts, and rollout operations docs for the universal mux I/O flight recorder.

## Architecture and Contracts

- `adr-0001-flight-recorder-architecture.md`
- `recorder-event-schema.md`
- `ft-recorder-event-v1.json`
- `storage-abstraction-backend-contract.md`
- `tantivy-schema-v1.md`
- `query-contract-v1.md`
- `semantic-chunking-windowing-policy-v1.md`
- `sequence-correlation-model.md`

## Governance and Safety

- `capture-redaction-policy.md`
- `capture-backpressure-overflow-policy.md`
- `recorder-governance-policy.md`
- `embedding-provider-governance-contract.md`

## Validation and Recovery

- `validation-gates-wa-oegrb-7-5.md`
- `security-privacy-validation-wa-oegrb-7-6.md`
- `recovery-drills-wa-oegrb-7-4.md`

## Rollout Track (`wa-oegrb.8.*`)

- `rollout-plan-wa-oegrb-8-1.md`
- `migration-plan-wa-oegrb-8-2.md`
- `recorder-governance-policy.md` (`wa-oegrb.8.3`, also listed above under Governance and Safety)
- `ops-runbook-wa-oegrb-8-4.md`
- `alerts-wa-oegrb-8-4.md`
- `incident-response-wa-oegrb-8-5.md`
- `adoption-handoff-wa-oegrb-8-6.md`

## Research / Dossiers

- `frankensqlite-append-log-dossier.md`
- `cass-two-tier-architecture-dossier.md`
- `xf-hybrid-retrieval-dossier.md`
- `cross-project-extraction-matrix.md`
- `feasibility-spike-results.md`
