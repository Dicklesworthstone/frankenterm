# Per-release headline-claim bench artifacts

Aggregated `wa-bench-meta.jsonl` outputs for the 5 benches backing
the headline performance claims. One file per release (and per
non-canonical CI lane), pinned by sha256 in the release
attestation bundle under the `perf/headline-claims` category.

## Schema

Each `<version>.headline-claims.json` is an object:

```json
{
  "release_version": "0.2.0",
  "ci_lane": "macos-14-m1",
  "git_commit": "<40-char hex>",
  "generated_at": "<UTC ISO-8601>",
  "source_manifest": "docs/perf/headline-claims.json",
  "bench_records": [
    /* one entry per bench run, schema-conformant to
       docs/perf/bench-stats-schema.json */
  ]
}
```

The canonical per-release artifact (no lane suffix) is produced
by the `macos-14-m1` lane — that lane matches the `ci_runner`
hardware baseline named in `docs/perf/headline-claims.json`. Other
lanes' files are uploaded as workflow artifacts only and are not
checked in; they exist for cross-lane regression triage.

## Generation cadence

Per repository release policy (`AGENTS.md` Rule 0.1), FrankenTerm uses
Doodlestein Self-Releaser (`dsr`) as its primary and exclusive release system
(never GitHub Actions):

- **On release build/publication (`dsr build` / `dsr release`):**
  Run all 5 benches on the target baseline lane via `dsr` / strict remote RCH
  proof workflows and commit the canonical `<version>.headline-claims.json`
  artifact to `main` alongside the signed DSR release attestation bundle.
- **Cross-lane regression triage:** non-canonical lane JSONL artifacts remain
  local/DSR receipt artifacts and are not checked into `docs/perf/releases/`.

## Cross-references

- Source manifest: [`docs/perf/headline-claims.json`](../headline-claims.json)
- Per-bench schema: [`docs/perf/bench-stats-schema.json`](../bench-stats-schema.json)
- Methodology: [`docs/perf/bench-stats-spec.md`](../bench-stats-spec.md)
- Audit harness: [`crates/frankenterm-core/tests/headline_claims_audit.rs`](../../../crates/frankenterm-core/tests/headline_claims_audit.rs)
- Release gate harness: [`scripts/release-gates.sh`](../../../scripts/release-gates.sh)
- Bead: `ft-vl7lp` (BR-RC-FOUNDATION.G3.5.cont.runners)
