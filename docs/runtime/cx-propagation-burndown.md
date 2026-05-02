# Cx-Propagation Burn-Down Dashboard

**Bead:** `ft-t9a6q.2` (BR-RC-RUNTIME-SEMANTICS.G14.2).
**Generator:** [`scripts/cx_propagation_burndown.py`](../../scripts/cx_propagation_burndown.py).
**Snapshot:** [`docs/runtime/cx-propagation.json`](cx-propagation.json).
**Trend:** [`docs/runtime/cx-propagation-trend.jsonl`](cx-propagation-trend.jsonl).
**Sibling lint:** [`cx-propagation-lint.md`](cx-propagation-lint.md) (ft-t9a6q.1).
**Sibling fixture:** [`labruntime-conventions.md`](labruntime-conventions.md) (ft-t9a6q.3).

This dashboard tracks `pub async fn` `&Cx` propagation across
`crates/frankenterm-core/src/` and reports it bucketed by the
parent bead's call-path criticality ranking. It is the
release-cadence reporting surface that complements:

- `ft-3kv6e` Python audit ratchet (script-time `&Cx` enforcement).
- `ft-t9a6q.1` cx-propagation analyzer (Rust-side AST analyzer; static analysis).
- `ft-t9a6q.3` LabRuntime fixture (runtime-time deterministic verification).

## Current state — sprint already complete

The Python audit ratchet under ft-3kv6e drove uncovered `pub
async fn` count to **0** before this dashboard landed. The
parent bead's "Cx-first refactor sprint" was completed
incrementally over the course of the ft-3kv6e adoption sweep.

This dashboard records that achievement and serves as the
**regression-guard**: if any future commit reintroduces an
uncovered `pub async fn`, the `--check` mode of the generator
exits 1 and CI fails the PR.

## Snapshot shape

`docs/runtime/cx-propagation.json` is overwritten on each run.
Its shape (schema v2):

```json
{
  "schema_version": 2,
  "generated_at": "<UTC ISO-8601>",
  "totals": {
    "total_sites": <int>,
    "covered_sites": <int>,
    "wrapper_exempt_sites": <int>,
    "exempt_file_sites": <int>,
    "uncovered_sites": <int>
  },
  "buckets": {
    "capture":     { "total", "covered", "wrapper_exempt", "exempt_file", "uncovered", "files": [...] },
    "workflow":    {...},
    "web_sse":     {...},
    "mcp":         {...},
    "distributed": {...},
    "connectors":  {...},
    "tx":          {...},
    "other":       {...}
  },
  "labruntime_coverage": {
    "total_call_sites": <int>,
    "files_with_calls": <int>,
    "tested_files_intersect_covered": <int>,
    "by_bucket": {
      "capture":     { "call_sites", "files_with_calls", "files": [...] },
      "workflow":    {...},
      "web_sse":     {...},
      "mcp":         {...},
      "distributed": {...},
      "connectors":  {...},
      "tx":          {...},
      "other":       {...}
    }
  }
}
```

### `labruntime_coverage`

Added in schema v2 under ft-y9wxt. Records adoption of the
LabRuntime fixture (ft-t9a6q.3) across the test corpus:

- **`total_call_sites`** — total `lab_runtime_test*` invocations
  across `crates/frankenterm-core/{src,tests}/` (counts all
  three entry points: `lab_runtime_test`,
  `lab_runtime_test_with_seed`, `lab_runtime_test_with_config`).
- **`files_with_calls`** — distinct `.rs` files containing ≥1
  invocation.
- **`tested_files_intersect_covered`** — coarse heuristic for
  "covered async fns that are exercised under LabRuntime" —
  count of files that BOTH contain a `lab_runtime_test*` call
  AND have at least one covered `pub async fn` per the audit's
  `by_file` map. Proximity-based, not 1:1.
- **`by_bucket`** — per-bucket breakdown matching the same
  8-bucket taxonomy as `buckets` above.

Files under `crates/frankenterm-core/tests/` are listed under a
synthetic `tests/<path>` prefix so they fall into the `other`
bucket by default. The audit script only walks `src/`, but the
LabRuntime adoption is most visible in `tests/`.

## Bucket taxonomy

Per the parent bead's call-path criticality ranking:

| Bucket | Path patterns | Rationale |
| ------ | ------------- | --------- |
| `capture` | `ingest_*`, `recorder_*`, `scan_pipeline.*`, `scrollback_*`, `capture_*`, `display_pipeline.*`, `differential_snapshot.*`, `continuous_backpressure.*`, `aegis_backpressure.*` | Drives the watch invariant from G9 — highest priority. |
| `workflow` | `workflows/*`, `workflow_*`, `plan.*`, `action_plan_*`, `dry_run.*` | Workflow engine — second priority. |
| `web_sse` | `web/*`, `webhook*`, `email_notify.*`, `desktop_notify.*` | Web/SSE surface — third priority. |
| `mcp` | `mcp/*`, `mcp_*` | MCP server — fourth priority. |
| `distributed` | `distributed_*`, `fleet_*`, `federation_*`, `headless_*`, `connector_mesh.*` | Distributed substrate — fifth priority. |
| `connectors` | `connector_*` (excl. `connector_mesh`) | Connector subsystem — sixth priority. |
| `tx` | `tx_*`, `mission_*`, `canary_rollout*`, `shadow_mode_*` | Transaction engine — seventh priority. |
| `other` | everything else | Untriaged — generally infrastructure / utility code. |

Order matters: a file matching multiple patterns is assigned to
the **first** bucket it matches, so `connector_mesh.rs` lands in
`distributed` (more specific intent) rather than `connectors`.

## Trend file

`docs/runtime/cx-propagation-trend.jsonl` is append-only. One
JSON object per line, schema:

```json
{
  "timestamp": "<UTC ISO-8601>",
  "totals": {...},
  "buckets": { "capture": {<no files>}, ... }
}
```

The `files` array is omitted from trend rows — it changes too
often to be informative on a release cadence. Run the snapshot
file through `git log -- docs/runtime/cx-propagation.json` to
see the file-level deltas.

## Operating cadence

### Local

```bash
# Snapshot only (don't append to trend)
scripts/cx_propagation_burndown.py --no-trend --print-summary

# Snapshot + trend append (operator default)
scripts/cx_propagation_burndown.py --print-summary

# Regression-guard mode (fails the build if uncovered > 0)
scripts/cx_propagation_burndown.py --check
```

### CI

The dashboard generator should run in two CI lanes:

1. **Per-PR, --check mode**: every PR runs the audit + dashboard
   generator with `--check`. Any reintroduction of uncovered
   `pub async fn` fails the lane. **Wired by `ft-gsgll`
   (BR-RC-RUNTIME-SEMANTICS.G14.2.cont.ci)** as a step in
   `.github/workflows/finish-line-guards.yml`'s `shell-guards`
   job — runs `python3 scripts/cx_propagation_burndown.py
   --check` on every PR + push to main. Step is cargo-free and
   completes in seconds; failure surfaces the exact uncovered
   site count.
2. **Weekly cron, snapshot + trend append**: a scheduled job
   runs the generator without `--check`, lets the snapshot
   refresh, appends a trend row, commits both files. Deferred to
   `ft-t9a6q.2.cont.cron`.

### Per-release attestation

Per the parent bead's "Report in attestation bundle per
release" criterion, the release pipeline copies the latest
`docs/runtime/cx-propagation.json` into the attestation bundle
under `attestation/cx-propagation.json`. Deferred to
`ft-t9a6q.2.cont.attestation` — wires the copy step into the
release script.

## Substrate vs wired-pass scope

Same substrate-pass / wired-pass split pattern as ft-t9a6q.1
and ft-t9a6q.3:

**Substrate-pass (this bead):**
- Dashboard generator at `scripts/cx_propagation_burndown.py`.
- Initial baseline snapshot at `docs/runtime/cx-propagation.json`.
- Trend file initialized at `docs/runtime/cx-propagation-trend.jsonl`.
- Regression test that asserts the schema + totals shape stay
  stable.
- This conventions doc.

**Wired-pass (named follow-ups):**
- `ft-t9a6q.2.cont.ci`: PR-CI lane wiring `--check` mode.
  **Landed via `ft-gsgll`** — see step
  "Run cx-propagation burndown gate (br-ft-gsgll)" in
  `.github/workflows/finish-line-guards.yml`.
- `ft-t9a6q.2.cont.cron`: weekly cron job.
- `ft-t9a6q.2.cont.attestation`: release-bundle copy step.
- `ft-t9a6q.2.cont.labruntime`: per-bead acceptance "LabRuntime
  test coverage tracked in attestation" — adds a complementary
  metric to the dashboard counting how many Cx-taking fns have
  a corresponding LabRuntime-fixture test under ft-t9a6q.3.

## Cross-references

- [`scripts/check_runtime_proof_coverage.py`](../../scripts/check_runtime_proof_coverage.py) — ft-3kv6e ratchet (the audit script this dashboard consumes).
- [`lints/cx_propagation/`](../../lints/cx_propagation/) — ft-t9a6q.1 Rust analyzer (alternative source of the same data).
- [`crates/frankenterm-core/src/test_fixtures/lab_runtime.rs`](../../crates/frankenterm-core/src/test_fixtures/lab_runtime.rs) — ft-t9a6q.3 LabRuntime fixture (runtime-time enforcer).
- [`runtime-proof-trait.md`](runtime-proof-trait.md) — the doctrine the dashboard certifies against.
- ft-t9a6q parent epic.
