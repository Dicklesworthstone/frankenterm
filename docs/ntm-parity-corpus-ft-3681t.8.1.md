# ft-3681t.8.1 NTM Parity Corpus and Acceptance Matrix

This document defines the canonical parity corpus for migration track `ft-3681t.8.*`.

## Objective

Provide machine-verifiable parity checkpoints between NTM operator workflows and FrankenTerm (`ft`) robot/session surfaces so downstream beads can gate migration on measurable evidence instead of ad-hoc manual checks.

## Artifacts

- Corpus: `fixtures/e2e/ntm_parity/corpus.v1.json`
- Acceptance matrix: `fixtures/e2e/ntm_parity/acceptance_matrix.v1.json`

## Corpus Structure

Each scenario defines:

- `id`: Stable parity scenario identifier (`NTM-PARITY-###`)
- `domain`: Functional area (state, pane I/O, waits, search, events, rules, snapshots, policy)
- `ntm_equivalent`: Legacy behavioral contract reference
- `ft_command`: Canonical `ft` command used for verification
- `success_assertions`: JSON-path-like checks that must pass for a successful run
- `failure_assertions`: Expected structured failure envelopes where applicable
- `artifact_key`: Required evidence bucket name

## Acceptance Matrix Gates

The matrix enforces four gate classes:

1. Blocking scenarios: All required IDs must pass (`max divergence = 0`).
2. High-priority scenarios: At least 90% pass, with at most one justified intentional delta.
3. Envelope contract: Every command must return either `ok=true` or a structured `error.code`.
4. Artifact contract: Every parity run must emit summary, raw outputs, and assertion results.

## How Downstream Beads Consume This

### `ft-3681t.8.2` (dual-run shadow comparator)

- Execute each `ft_command` in the corpus against shadow fixtures/live targets.
- Write run outputs to `artifacts/e2e/ntm_parity/<run_id>/`.
- Emit `assertion_results.json` using the matrix schema.

### `ft-3681t.8.4` (staged cutover playbook)

- Use matrix gate results as explicit cutover go/no-go inputs.
- Carry over intentional-delta decisions into cutover risk register.

## Machine-Verifiable Contract

A run is valid only if:

- Every executed scenario has a result object with fields:
  - `scenario_id`
  - `status` (`pass|fail|intentional-delta|untested`)
  - `artifacts` (non-empty list)
- Artifacts include:
  - `summary.json`
  - `raw_command_outputs.jsonl`
  - `assertion_results.json`

## Current Limitations / Dependencies

- This corpus is drafted early by design (per `docs/ft-3681t-execution-plan.md`) and should evolve as `ft-3681t.4.4`, `ft-3681t.5.9`, and `ft-3681t.9.2` land.
- Scenario IDs are stable; scenario internals may evolve with explicit schema-version bumps.

## Next Updates

1. Add concrete fixture-backed expected outputs for each scenario where deterministic snapshots are available.
2. Add comparator-side harness wiring (`ft-3681t.8.2`) to consume this corpus directly.
3. Add migration dashboard rollups for matrix status by domain and severity.
