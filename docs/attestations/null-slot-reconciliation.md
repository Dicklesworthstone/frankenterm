# Null-Slot Reconciliation

**Bead:** `ft-e87u6.1`  
**Generated:** 2026-05-09T21:37:04Z  
**Manifest:** `docs/attestations/manifest.json` (`sha256:608ee741282e79dcf041e0efe49639f91200b17b3c594b7fc0cbbc96f9eb48a6`)

This worksheet covers every `path: null` slot currently declared in
`docs/attestations/manifest.json`. It does not update the manifest; it
classifies the slots so `ft-e87u6.2` can either populate paths or record
the recovery bead deliberately.

## Summary

| Outcome | Count |
| --- | ---: |
| Null slots examined | 9 |
| `populate-from` | 3 |
| `substrate-recovery` | 6 |
| `deferred` | 0 |
| `slot-deletion` | 0 |
| `human_review_required` | 0 |

## Worksheet

| Slot | Status | Artifact path or follow-up bead | Evidence paste |
| --- | --- | --- | --- |
| `perf/headline-claims` | `populate-from` | `docs/perf/headline-claims.json` | `br show ft-syqcz.3` is closed; `git show f3e235393a68303c408efca22f9c0183354e123d --name-only` lists the JSON; `jq` parses it. |
| `perf/competitor-matrix` | `substrate-recovery` | `ft-e87u6.9` | Manifest points at `ft-syqcz.4`, but that commit only shipped network-calculus substrate. The competitor work is under `ft-syqcz.5` / `ft-t101b`; no checked-in per-release matrix JSON exists. |
| `tui/render-parity` | `substrate-recovery` | `ft-e87u6.10` | `ft-35yac.2` shipped the ftui default cutover. The existing `docs/attestations/tui/render-parity-gpu.json` is a separate GPU adjunct and explicitly leaves the full byte-level report to `ft-35yac.2`. |
| `security/passive-watch` | `substrate-recovery` | `ft-e87u6.11` | `ft-x0666.1` shipped invariant code, fuzz target, corpus, and `docs/security/passive-watch-attestation.md`; the manifest expects `application/json` and no passive-watch JSON exists. |
| `security/redactor-coverage` | `populate-from` | `docs/security/redactor-coverage.json` | `git show c3bf363cdbdec4fe5b11d677b47a7f8509f30aab --name-only` lists the JSON; it parses and names `bead: ft-x0666.2`. |
| `security/distributed-threat-model` | `populate-from` | `docs/security/distributed-threat-model.md` | The manifest media type is `text/markdown`; `git show 974ed1af38ee1d20d8d00ed1b20f7bf655d00b67 --name-only` lists the threat model markdown. |
| `proofs/loom-runtime-async` | `substrate-recovery` | `ft-e87u6.12` | `ft-syqcz.6` shipped Loom skeletons/docs and `ft-syqcz.7` closed the full proof corpus, but no machine-readable JSON proof artifact exists. |
| `proofs/runtime-proof-trait` | `substrate-recovery` | `ft-e87u6.13` | Live `br show ft-i2eni.1` now reports closed. The current source commit is `34b447b44`; it shipped `runtime_proof.rs` and markdown doctrine, but not a category-specific JSON attestation. |
| `doctrine/agents-md-counts` | `substrate-recovery` | `ft-e87u6.14` | `ft-i2eni.5` shipped the stamper, placeholders, CI check, and docs. The stamper has a seven-entry manifest but publishes no checked-in JSON snapshot. |

## Follow-Up Beads Filed

| Bead | Slot | Required output |
| --- | --- | --- |
| `ft-e87u6.9` | `perf/competitor-matrix` | Concrete per-release competitor matrix JSON or an intentional manifest provenance/semantic correction. |
| `ft-e87u6.10` | `tui/render-parity` | Full ratatui-to-ftui parity JSON, distinct from the populated GPU adjunct. |
| `ft-e87u6.11` | `security/passive-watch` | PassiveWatchHealth/fuzz-proof JSON artifact matching `application/json`. |
| `ft-e87u6.12` | `proofs/loom-runtime-async` | Machine-readable Loom proof summary for runtime_async primitives. |
| `ft-e87u6.13` | `proofs/runtime-proof-trait` | Machine-readable RuntimeProof seal/adoption attestation. |
| `ft-e87u6.14` | `doctrine/agents-md-counts` | Checked-in JSON snapshot for the seven tracked README/AGENTS counts. |

The machine-readable version is
`docs/attestations/null-slot-reconciliation.json`; the E2E harness at
`tests/e2e/test_ft_e87u6_1_null_slot_reconciliation.sh` validates it
against the live manifest, filesystem, hashes, and Beads state.
