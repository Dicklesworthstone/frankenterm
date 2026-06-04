# Attention Router Fixtures

This directory contains the retained attention-router scenario inventory and
live surface goldens. The scenario inventory feeds the broader golden harness
(`ft-x3nsb.6`). The `surface-status.golden.json` and
`surface-status.golden.toon` files retain the `ft-x3nsb.4` read-only status
surface shape for CLI, Robot, and MCP parity checks. The source fixture for
those examples is `source-adapter-input.ready.v1.json`.

The nudge-plan inventory in `nudge-plan-receipts.v1.json` pins the retained
receipt vocabulary for `ft-x3nsb.5`: ack-required messages, thread replies,
stale-claim status checks, dirty-overlap handoffs, force-release review only,
and proof-starved no-action cases. Live attention items embed the same
side-effect-free receipt vocabulary through `nudge_plan_receipt`.

The fixture contract is `docs/json-schema/ft-attention-router-scenarios.json`.
It is a hand-authored Draft 2020-12 schema tracked in
`docs/json-schema/PROVENANCE.md`.

Future harnesses should turn `scenarios.v1.json` into scenario-specific JSON
and TOON goldens by collecting reduced command artifacts, canonicalizing
dynamic fields, and comparing the router output against each scenario's
expected classification and safe action. The retained surface goldens pin the
input-backed ready status envelope and are checked by
`frankenterm_core::attention_router` tests. Fixtures must not store raw pane
text, secrets, full Agent Mail message bodies, or unsanitized build logs.

`ft-x3nsb.6.3` extends the same inventory with the reservation firewall matrix:
active exclusive reservation overlap, release-message-without-release,
ownership source disagreement, local closeout awaiting publication, and stale
owner status-check-before-force-release. These cases are deliberately kept in
the shared scenario inventory so the future JSON and TOON golden harness
classifies ownership hazards with the same vocabulary as the rest of the
attention router.

`ft-x3nsb.6.4` adds the local disk-pressure approval scenario. It preserves the
case where ENOSPC blocks writes, read-only cleanup inventory identifies
candidate temporary/build directories, and no destructive cleanup may run until
the user supplies the exact command and explicit irreversible consent.

The scenario and nudge-plan inventories are static-proof inputs; the retained
surface goldens are also checked by compiled Rust tests and require RCH proof
for closeout. `tests/e2e/test_attention_router_scenarios.sh` emits one JSONL
record per scenario with the retained classification, safe action, reason
codes, explanation terms, and volatility level so proof logs explain why each
case was accepted. Validating this directory requires:

```bash
jq empty docs/json-schema/ft-attention-router-scenarios.json
jq empty fixtures/attention-router/scenarios.v1.json
jq empty fixtures/attention-router/source-adapter-input.ready.v1.json
jq empty fixtures/attention-router/surface-status.golden.json
bash fixtures/attention-router/verify-nudge-plan-receipts.sh
bash tests/e2e/test_attention_router_scenarios.sh
jsonschema -i fixtures/attention-router/scenarios.v1.json docs/json-schema/ft-attention-router-scenarios.json
git diff --check -- docs/json-schema/ft-attention-router-scenarios.json docs/json-schema/PROVENANCE.md fixtures/attention-router
br dep cycles --json
```

If the `jsonschema` CLI is not installed, the JSON parse and provenance checks
are still useful static proof, but the schema-validation command must be rerun
before any harness claims conformance.

Do not use local Cargo as proof for this lane. Do not repair or restart Agent
Mail, restart RCH, cancel builds, mutate workers, delete files, or touch dirty
paths owned by another agent while collecting these fixtures.
