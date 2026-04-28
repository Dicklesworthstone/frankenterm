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
3. `tests/conformance_mcp_coverage.rs` parses this document, extracts
   each clause ID, greps the test corpus, and **fails the build** if
   any clause has zero matching annotations.

## Workflow when adding a clause

When `docs/mcp-api-spec.md` grows a new MUST / SHOULD line:

1. Add a row below with the next free `MCP-V1-NNN` id.
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
| `MCP-V1-001` | Response Envelope (v1) | MUST | TESTED | `data` matches the corresponding robot JSON schema under `docs/json-schema/`. | [`mcp-api-spec.md:45`](mcp-api-spec.md) | `tests/conformance_robot_envelope_schema.rs` (ft-5ikbd, 04911fff7) |
| `MCP-V1-002` | wa.workflow_status | MUST | DEFERRED | At least one of `execution_id`, `pane_id`, or `active` must be provided. | [`mcp-api-spec.md:182`](mcp-api-spec.md) | `wa.workflow_status` is not yet registered as an MCP tool; the clause is in spec but the surface to enforce it doesn't exist. **Tracked as a follow-up bead** — when the tool is registered, add a test that calls it with all three filters absent and asserts an `FT-MCP-0001` envelope rejection. |
| `MCP-V1-003` | Safety & Policy | MUST | TESTED | Any tool that causes side effects MUST pass the PolicyEngine (`wa.send`, `wa.workflow_run`/`wa.workflow_abort`, `wa.approve`, `wa.reserve`/`wa.release`, `wa.accounts_refresh`). | [`mcp-api-spec.md:235`](mcp-api-spec.md) | `tests/mcp_conformance_core_tools.rs` (`mcp_conformance_wa_send_contract_matches_golden`) — wa.send is the canonical side-effect tool, golden pins its policy-gated envelope shape. |
| `MCP-V1-004` | Safety & Policy | MUST | TESTED | Resources are read-only and MUST not cause side effects. | [`mcp-api-spec.md:242`](mcp-api-spec.md) | `tests/mcp_conformance.rs` (`mcp_conformance_resource_catalog_is_versioned_json_for_clients`, `mcp_conformance_rules_resource_returns_well_formed_json_envelope`, `mcp_conformance_workflows_resource_returns_counted_json_payload`) |
| `MCP-V1-005` | Parity & Schema Contract | MUST | TESTED | Output `data` must validate against the matching robot JSON schema. | [`mcp-api-spec.md:254`](mcp-api-spec.md) | `tests/conformance_robot_envelope_schema.rs` (same enforcement path as MCP-V1-001) |
| `MCP-V1-006` | Parity & Schema Contract | MUST | TESTED | Errors must map to stable MCP error codes (`FT-MCP-0001` … `FT-MCP-0006`). | [`mcp-api-spec.md:255`](mcp-api-spec.md) | `tests/mcp_conformance.rs:93`, `tests/mcp_conformance_core_tools.rs:400`, `tests/mcp_conformance_rules_test.rs:430` (assert `envelope["error_code"] == "FT-MCP-0001"` etc.) |

**Score: 5 / 6 MUST clauses tested. 1 deferred (MCP-V1-002, blocked on tool registration).**

The CI gate `tests/conformance_mcp_coverage.rs` reads this matrix
and asserts every TESTED clause has a matching `MCP-V1-NNN`
annotation in the test corpus. DEFERRED clauses are tracked but do
not fail the build; deleting their row (without registering the
tool) is what fails it.

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
