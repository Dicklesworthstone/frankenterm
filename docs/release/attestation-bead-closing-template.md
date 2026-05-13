# Attestation-Producing Bead Closing Template

Paste this shape into the closing comment for any bead that creates or updates
an attestation artifact. Fill every placeholder before closing.

## Attestation closure (per ft-187kv + ft-e87u6.6)

- Manifest slot category: `<category>`
- Artifact path: `<path>` (sha256 `<hash>`)
- Build smoke: `bash scripts/attestation-build.sh --version 0.0.0-dev --channel dev --sign unsigned` exit `<code>`
- Strict-deferred build: `bash scripts/attestation-build.sh ... --strict-deferred` exit `<code>` (must be 0 if any previously-deferred slot now resolves)
- Verify round-trip: `bash scripts/attestation-verify.sh <bundle>` exit `<code>`
- Hedge alignment: `cargo test -p frankenterm-core --test readme_hedge_alignment` exit `<code>` (per ft-e87u6.4)
- Manifest completeness: `cargo test -p frankenterm-core --test attestation_manifest_completeness` exit `<code>` (per ft-e87u6.5)
- RCH artifact bundle: `<path>`

If the bead does not change README or AGENTS wording, keep the Hedge alignment
line and record `not_applicable` as the exit value rather than removing the
line. The template is intentionally stable so downstream checks can grep the
same field names.
