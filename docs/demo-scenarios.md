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

## Operator Workflow

The bundled demo surface is intentionally conservative:

```bash
ft demo
ft demo quickstart
ft demo quickstart --format json
ft demo quickstart --format toon
ft demo quickstart --manifest fixtures/demo-lab/manifest.v1.json
```

Omitting the demo name lists the manifest scenarios and their availability.
Running a named demo validates the selected scenario through the same
`Scenario::load` contract used by `ft simulate validate`; it does not send
input to live panes, call external providers, repair services, or start proof
lanes. The named demo output includes the exact follow-up command shapes:

```bash
ft simulate validate fixtures/demo-lab/scenarios/quickstart.yaml
ft simulate run fixtures/demo-lab/scenarios/quickstart.yaml --speed 1
```

Use `ft demo <name>` for onboarding and retained artifact discovery. Use
`ft simulate validate <scenario>.yaml --json` when you only need scenario
syntax/metadata validation, and use `ft simulate run <scenario>.yaml` when you
want the mock-WezTerm simulation playback path. None of these demo-lab commands
is target-class capacity proof.

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
  Retained proof-ledger and proof-summary artifacts may omit the manifest-level
  `sha256` pin because they record the current manifest hash internally; the
  static verifier compares those internal hashes to the live files and compares
  the proof-summary ledger hash to the ledger on disk.
- `degradation`: explicit behavior for Agent Mail unavailable, disabled
  features, RCH proof unavailable, and unsupported platforms.

## Retained Proof Harness

The no-mock retained proof harness lives in
`tests/e2e/test_demo_lab_fixture_manifest.sh`. It validates the manifest,
scenario YAML, retained JSON/TOON goldens, negative fragments, and the shared
proof artifacts:

- `fixtures/demo-lab/proof/proof-ledger.v1.jsonl`
- `fixtures/demo-lab/proof/summary.v1.json`

The proof ledger has one JSONL entry per bundled scenario. Each entry records
the normalized `ft demo <scenario> --format json`, `ft demo <scenario>
--format toon`, and `ft simulate validate <scenario> --json` command shapes,
exit codes, stdout/stderr retention state, manifest hash, scenario hash,
target-dir/worker placeholders, whether remote Cargo/rustc/test was reached,
and side-effect flags. The summary records the ledger hash and makes the proof
boundary explicit: these artifacts prove deterministic fixture and
CLI/simulation command contracts only. They do not prove remote Cargo,
target-class capacity, live-pane mutation, or production-scale behavior.

## Bundled Scenarios and Artifacts

| Demo | Scenario | Retained artifacts | Proof commands |
|---|---|---|---|
| `quickstart` | `fixtures/demo-lab/scenarios/quickstart.yaml` | `fixtures/demo-lab/golden/quickstart.json`, `fixtures/demo-lab/golden/quickstart.toon`, `fixtures/demo-lab/proof/proof-ledger.v1.jsonl`, `fixtures/demo-lab/proof/summary.v1.json` | `ft demo quickstart --manifest fixtures/demo-lab/manifest.v1.json --format json`; `ft demo quickstart --manifest fixtures/demo-lab/manifest.v1.json --format toon`; `ft simulate validate fixtures/demo-lab/scenarios/quickstart.yaml --json` |
| `usage_limit` | `fixtures/demo-lab/scenarios/usage_limit.yaml` | `fixtures/demo-lab/golden/usage_limit.json`, `fixtures/demo-lab/proof/proof-ledger.v1.jsonl`, `fixtures/demo-lab/proof/summary.v1.json` | `ft demo usage_limit --manifest fixtures/demo-lab/manifest.v1.json --format json`; `ft demo usage_limit --manifest fixtures/demo-lab/manifest.v1.json --format toon`; `ft simulate validate fixtures/demo-lab/scenarios/usage_limit.yaml --json` |
| `compaction` | `fixtures/demo-lab/scenarios/compaction.yaml` | `fixtures/demo-lab/golden/compaction.toon`, `fixtures/demo-lab/proof/proof-ledger.v1.jsonl`, `fixtures/demo-lab/proof/summary.v1.json` | `ft demo compaction --manifest fixtures/demo-lab/manifest.v1.json --format json`; `ft demo compaction --manifest fixtures/demo-lab/manifest.v1.json --format toon`; `ft simulate validate fixtures/demo-lab/scenarios/compaction.yaml --json` |

The proof ledger records that some JSON or TOON stdout shapes are retained as
goldens and others are only normalized command contracts. Treat the table above
as an artifact index, not as a promise that every format for every scenario has
a committed golden file.

## Degradation Behavior

Every scenario must define the same four degradation classes:

| Reason | Status | Required behavior |
|---|---|---|
| `agent_mail_unavailable` | `degraded` | Continue with Beads and git evidence; do not repair, reconstruct, stop, or restart Agent Mail. |
| `disabled_feature` | `unavailable` | Return the disabled feature in the machine envelope and skip execution without side effects. |
| `rch_proof_unavailable` | `proof_blocked` | Retain preflight/static evidence only; do not count local Cargo output as proof. |
| `unsupported_platform` | `unavailable` | Report the target/platform and leave the scenario unrun. |

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
- malformed pinned artifact SHA-256 values
- secret-shaped text in manifest strings
- manifest proof boundaries that overclaim target-class production capacity

`ft demo` loads and validates the manifest before listing or running a scenario.
Validation failure must surface as a typed machine-readable error. It must not
fall back to a marketing demo, ignore missing artifacts, or count local Cargo
output as RCH proof.

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
