# Release Attestation Signing

FrankenTerm release attestation bundles are signed with Sigstore cosign keyless
signing in the release workflow. The Ed25519 mode remains only as an offline
fallback for builds that cannot reach the Sigstore public-good infrastructure.

## Release Signer

The only CI identity allowed to sign production release bundles is the
repository release workflow running on the release tag ref:

```text
https://github.com/<owner>/<repo>/.github/workflows/release.yml@refs/tags/v<version>
```

The workflow writes the concrete identity into
`signature.certificate_identity`. The expected OIDC issuer is:

```text
https://token.actions.githubusercontent.com
```

That value is written into `signature.certificate_oidc_issuer`.

## Trust Root And Log

Production keyless signatures use the Sigstore public instance:

- Fulcio issues the short-lived signing certificate for the GitHub Actions OIDC
  identity.
- Rekor records the signing event in the transparency log.
- Cosign writes the signature, certificate, and Rekor verification material into
  `docs/attestations/<version>.sigstore`.

The release bundle records the `.sigstore` file under
`signature.sigstore_bundle` with the repo-relative path, SHA-256, and byte
length. Verification fails if that external file is missing or its bytes no
longer match the signed JSON bundle's metadata.

Reference docs:

- Sigstore blob signing:
  <https://docs.sigstore.dev/cosign/signing/signing_with_blobs/>
- Sigstore keyless blob verification:
  <https://docs.sigstore.dev/cosign/verifying/verify/>

## Third-Party Verification

Use the repository verifier for the normal offline release check:

```bash
scripts/attestation-verify.sh docs/attestations/<version>.json --strict-required --strict-deferred
```

The verifier re-hashes every listed artifact, recomputes the canonical signing
payload, checks the `.sigstore` file's hash and size, then invokes:

```bash
cosign verify-blob \
  --bundle docs/attestations/<version>.sigstore \
  --certificate-identity "https://github.com/<owner>/<repo>/.github/workflows/release.yml@refs/tags/v<version>" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  <canonical-payload-file>
```

`ft attestation verify docs/attestations/<version>.json` delegates to the same
script, so the CLI and shell verifier enforce the same Sigstore bundle checks.
