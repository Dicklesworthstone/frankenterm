# Attention Router Fixtures

This directory contains the retained scenario inventory for the future
attention-router golden harness (`ft-x3nsb.6`). It is intentionally static:
the inventory freezes the cases the router must classify before the Rust/CLI
surface exists, while `ft-4tp7g` keeps remote Cargo proof blocked.

Future harnesses should turn `scenarios.v1.json` into generated JSON and TOON
goldens by collecting reduced command artifacts, canonicalizing dynamic fields,
and comparing the router output against each scenario's expected
classification and safe action. The fixture contract records only
reason-code-level evidence. It must not store raw pane text, secrets, full
Agent Mail message bodies, or unsanitized build logs.

The inventory is static-proof only. Validating this directory requires:

```bash
jq empty fixtures/attention-router/scenarios.v1.json
git diff --check -- fixtures/attention-router
br dep cycles --json
```

Do not use local Cargo as proof for this lane. Do not repair or restart Agent
Mail, restart RCH, cancel builds, mutate workers, delete files, or touch dirty
paths owned by another agent while collecting these fixtures.
