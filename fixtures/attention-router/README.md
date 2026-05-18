# Attention Router Fixtures

This directory contains the retained scenario inventory for the future
attention-router golden harness (`ft-x3nsb.6`). It is intentionally static:
the inventory freezes the cases the router must classify before the Rust/CLI
surface exists, while `ft-4tp7g` keeps remote Cargo proof blocked.

The fixture contract is `docs/json-schema/ft-attention-router-scenarios.json`.
It is a hand-authored Draft 2020-12 schema tracked in
`docs/json-schema/PROVENANCE.md`.

Future harnesses should turn `scenarios.v1.json` into generated JSON and TOON
goldens by collecting reduced command artifacts, canonicalizing dynamic fields,
and comparing the router output against each scenario's expected
classification and safe action. The fixture contract records only
reason-code-level evidence. It must not store raw pane text, secrets, full
Agent Mail message bodies, or unsanitized build logs.

`ft-x3nsb.6.3` extends the same inventory with the reservation firewall matrix:
active exclusive reservation overlap, release-message-without-release,
ownership source disagreement, local closeout awaiting publication, and stale
owner status-check-before-force-release. These cases are deliberately kept in
the shared scenario inventory so the future JSON and TOON golden harness
classifies ownership hazards with the same vocabulary as the rest of the
attention router.

The inventory is static-proof only. Validating this directory requires:

```bash
jq empty docs/json-schema/ft-attention-router-scenarios.json
jq empty fixtures/attention-router/scenarios.v1.json
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
