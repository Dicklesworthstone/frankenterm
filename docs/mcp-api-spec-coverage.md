# MCP API Spec — Conformance Coverage Matrix

> Per-clause coverage map for `docs/mcp-api-spec.md`. Required by the
> [/testing-conformance-harnesses](https://github.com/jeffrey-emanuel/jeffreys-skills.md)
> "Coverage Accounting Matrix (Mandatory)" rule and tracked under
> bead **ft-zaqi8**.

## How this works

1. Every normative clause (MUST / SHOULD / REQUIRED) in
   `docs/mcp-api-spec.md` is enumerated below with a stable ID
   (`MCP-V1-NNN`).
2. Each clause names the test(s) that enforce it. Tests carry a
   `// MCP-V1-NNN` annotation comment so they are grep-discoverable
   from the matrix.
3. `tests/conformance_mcp_coverage.rs` parses the spec and this document,
   checks their normative IDs are identical, greps unit and integration tests,
   and **fails the build** if any tested clause has zero annotations.

## Workflow when adding a clause

When `docs/mcp-api-spec.md` grows a new MUST / SHOULD line:

1. Put a unique `<!-- MCP-V1-NNN -->` ID on the normative spec line and add the
   matching row below.
2. Identify (or add) a test that enforces the clause; annotate it
   with `// MCP-V1-NNN` so the gate finds it.
3. `cargo test -p frankenterm-core --test conformance_mcp_coverage`
   passes only when every clause in this matrix is annotated by at
   least one test.

A PR that adds a clause **without** updating the matrix and adding
the annotation will fail CI on this gate.

## Coverage matrix (v1)

| ID | Section | Level | Status | Clause | Spec line | Tested by |
|----|---------|:-----:|:------:|--------|----------:|-----------|
| `MCP-V1-001` | Response Envelope (v1) | MUST | TESTED | When the tool table names a JSON schema, `data` matches that schema. | [`mcp-api-spec.md:46`](mcp-api-spec.md) | `tests/conformance_robot_envelope_schema.rs` (ft-5ikbd, 04911fff7) |
| `MCP-V1-002` | wa.workflow_status | CONTRACT | TESTED | At least one of `execution_id`, `pane_id`, or `active` must be provided. | [`mcp-api-spec.md:201`](mcp-api-spec.md) | `mcp::mcp_tools::tests::workflow_status_requires_filter_param` asserts missing filters return an `FT-MCP-0001` envelope; `mcp::mcp_tools::tests::workflow_status_definition_declares_filter_requirement` pins the schema guard. |
| `MCP-V1-003` | Safety & Policy | MUST | TESTED | Any tool that causes side effects MUST pass the PolicyEngine (`wa.send`, `wa.workflow_run`/`wa.workflow_abort`, `wa.approve`, `wa.reserve`/`wa.release`, `wa.accounts_refresh`). | [`mcp-api-spec.md:275`](mcp-api-spec.md) | `tests/mcp_conformance_core_tools.rs` (`mcp_conformance_wa_send_contract_matches_golden`) — wa.send is the canonical side-effect tool, golden pins its policy-gated envelope shape. |
| `MCP-V1-004` | Safety & Policy | MUST | TESTED | Resources are read-only and MUST not cause side effects. | [`mcp-api-spec.md:282`](mcp-api-spec.md) | `tests/mcp_conformance.rs` (`mcp_conformance_resource_catalog_is_versioned_json_for_clients`, `mcp_conformance_rules_resource_returns_well_formed_json_envelope`, `mcp_conformance_workflows_resource_returns_counted_json_payload`) |
| `MCP-V1-005` | Parity & Schema Contract | RETIRED | RETIRED | Superseded duplicate of `MCP-V1-001`; the old blanket Robot-parity wording was removed. | — | Historical annotation retained so old proof references remain resolvable. |
| `MCP-V1-006` | Parity & Schema Contract | CONTRACT | TESTED | Every MCP error maps to a stable code from the published catalog. | [`mcp-api-spec.md:297`](mcp-api-spec.md) | `tests/mcp_conformance.rs`, `tests/mcp_conformance_core_tools.rs`, and `tests/mcp_conformance_rules_test.rs` assert stable envelope error codes. |

**Score: 3 / 3 explicit MUST clauses tested; 2 / 2 additional contract clauses tested; 1 historical clause retired.**

The CI gate `tests/conformance_mcp_coverage.rs` reads this matrix and the spec.
It asserts that every explicit MUST/SHOULD/REQUIRED clause carries a stable
`MCP-V1-NNN` annotation, that its ID maps one-to-one to a normative matrix row,
and that every TESTED clause has a matching annotation in the crate's unit or
integration test corpus. DEFERRED clauses are tracked but do not fail the
test-annotation gate.

## Out of matrix (intentional)

The spec contains additional shape-level requirements that are
*implicit* (Markdown tables of fields, JSON snippet schemas) rather
than explicit MUST / SHOULD prose. Those are covered by the per-tool
golden suites in `tests/mcp_conformance_*.rs` and the manifest
goldens in `tests/mcp_manifest_golden.rs`. The matrix above tracks
only the explicit normative clauses; deciding whether an implicit
shape requirement deserves its own matrix row is a judgment call —
when in doubt, add a row.

## CI gate

The gate test (`tests/conformance_mcp_coverage.rs`) runs on every
PR via the standard `cargo test` lane. A clause without coverage
fails with output identifying the orphan ID:

```
clauses missing test annotations: ["MCP-V1-007"]
add a `// MCP-V1-007` comment to a test that enforces the clause,
then re-run.
```

## References

- `docs/mcp-api-spec.md` (the spec being mapped)
- `tests/conformance_mcp_coverage.rs` (the CI gate)
- `tests/conformance_robot_envelope_schema.rs` (ft-5ikbd, runtime
  schema validator)
- Skill: testing-conformance-harnesses, "Coverage Accounting Matrix"
- Bead: ft-zaqi8
