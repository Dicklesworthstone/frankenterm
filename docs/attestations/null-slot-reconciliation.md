# Null-Slot Reconciliation

**Bead:** `ft-e87u6.1`
**Refreshed:** 2026-05-18T12:47:50Z
**Manifest:** `docs/attestations/manifest.json` (`sha256:a30cb1bbb647d9f55368825c3d1c55e0f0811e12b596d74d74d1a8f4c0daa062`)

This worksheet is the current live reconciliation for `path: null` slots in
`docs/attestations/manifest.json`. Earlier revisions of this file recorded
the nine-slot recovery worksheet used by `ft-e87u6.1`; those rows have now
been consumed by the recovery and manifest-wiring beads. The live manifest
currently has no null-path slots.

## Summary

| Outcome | Count |
| --- | ---: |
| Null slots examined | 0 |
| `populate-from` | 0 |
| `substrate-recovery` | 0 |
| `deferred` | 0 |
| `slot-deletion` | 0 |
| `human_review_required` | 0 |

## Current Manifest State

The current manifest declares concrete paths for every slot. In particular,
`doctrine/agents-md-counts` now points at
`docs/attestations/doctrine/agents-md-counts.json`, which is the checked-in
JSON snapshot produced after the original substrate-recovery row.

## Historical Recovery Rows

The original nine-row worksheet remains available in Git history. Its follow-up
beads were:

| Bead | Slot | Outcome |
| --- | --- | --- |
| `ft-e87u6.9` | `perf/competitor-matrix` | Recovery row superseded by manifest wiring. |
| `ft-e87u6.10` | `tui/render-parity` | Recovery row superseded by manifest wiring. |
| `ft-e87u6.11` | `security/passive-watch` | Recovery row superseded by manifest wiring. |
| `ft-e87u6.12` | `proofs/loom-runtime-async` | Recovery row superseded by manifest wiring. |
| `ft-e87u6.13` | `proofs/runtime-proof-trait` | Recovery row superseded by manifest wiring. |
| `ft-e87u6.14` | `doctrine/agents-md-counts` | Resolved by `docs/attestations/doctrine/agents-md-counts.json`. |

The machine-readable current-state sidecar is
`docs/attestations/null-slot-reconciliation.json`; the E2E harness at
`tests/e2e/test_ft_e87u6_1_null_slot_reconciliation.sh` validates it
against the live manifest, filesystem, hashes, and Beads state.
