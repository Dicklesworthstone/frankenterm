# Bundled Demo Scenario Contract

This document is the operator-facing companion for the
`ft.demo.scenario-manifest.v1` contract implemented in
`frankenterm_core::demo_scenarios`.

The manifest exists because `ft demo` advertises named scenarios
(`quickstart`, `usage_limit`, and `compaction`) that should become bundled,
deterministic assets rather than prose-only placeholders. The first contract
fixture lives at `fixtures/demo-lab/manifest.v1.json`.

## Boundaries

Bundled demo scenarios are onboarding and regression fixtures. They can prove
that a named scenario validates, runs through the intended CLI/simulation path,
emits bounded machine-readable output, and retains expected artifacts. They are
not target-class production-capacity evidence and must not be used to promote
200+ pane or 64-core / 256 GiB claims.

The demo runner must not repair or restart Agent Mail, cancel RCH builds,
delete or mutate remote source mirrors, perform destructive git cleanup, or
send input to live panes unless a later explicitly approval-gated live-demo
bead adds that behavior.

## Manifest Fields

Each manifest declares:

- `schema_version`: currently `ft.demo.scenario-manifest.v1`.
- `title`: human-readable manifest title.
- `proof_boundary`: explicit statement of what the scenarios do and do not
  prove.
- `scenarios`: one entry per bundled `ft demo <name>` asset.

Each scenario declares:

- `id`: stable CLI id. Only lowercase ASCII, digits, `_`, and `-`.
- `title` and `purpose`: operator-facing text.
- `scenario_path`: relative path to the scenario YAML asset.
- `deterministic_seed`: seed used by generated fixture data.
- `required_features`: non-empty list of required runtime or CLI features.
- `supported_outputs`: supported output formats (`human`, `json`, `toon`,
  `jsonl`).
- `redaction_tier`: required redaction tier before artifacts can ship.
- `proof_category`: `conformance`, `golden`, or `e2e`.
- `max_output_bytes`: maximum total output budget.
- `expected_artifacts`: bounded, relative artifact paths with per-artifact
  byte budgets and content-hash requirements. Committed artifacts other than
  the self-referential manifest entry must carry a pinned lowercase SHA-256
  hash in `sha256`; the static verifier compares that pin to the file on disk.
  Future retained proof artifacts may omit the pin until the artifact exists.
- `degradation`: explicit behavior for Agent Mail unavailable, disabled
  features, RCH proof unavailable, and unsupported platforms.

## Versioning

`ft.demo.scenario-manifest.v1` is a pre-1.0 contract. A breaking field rename,
semantic change, or new required invariant must introduce a new schema version
and keep a validator for the previous version until all bundled manifests have
migrated. A v1 manifest may add optional fields only after the validator has a
fail-closed rule for unknown critical behavior.

## Validation Rules

The core validator rejects:

- unsupported schema versions
- empty manifests
- duplicate or unstable ids
- absolute paths, paths containing `..`, or platform-specific path syntax
- non-YAML scenario paths
- empty required lists
- missing degradation reason codes
- zero or oversized artifact budgets
- secret-shaped text in manifest strings

When the demo runner later wires this contract into `ft demo`, validation
failure should surface as a typed machine-readable error. It should not fall
back to a marketing demo, ignore missing artifacts, or count local Cargo output
as RCH proof.

## Negative Fixtures

The retained negative fragment corpus lives at
`fixtures/demo-lab/invalid/manifest-fragments.v1.json`. It is parseable JSON
that the static verifier must reject by contract shape rather than by syntax.
The required cases are:

- `unsupported-schema-version`
- `absolute-scenario-path`
- `parent-relative-artifact-path`
- `missing-degradation-reason`
- `duplicate-scenario-id`
- `target-class-proof-overclaim`

These fragments prove that the demo-lab contract stays fail-closed for version
drift, path escape, incomplete degradation guidance, unstable scenario identity,
and inflated production-capacity claims.
