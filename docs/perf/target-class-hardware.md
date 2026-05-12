# Target-Class Hardware Contract

Date: 2026-05-12
Bead: `ft-tf6g3.14`
Status: fail-closed target-class proof gate for the resource-pressure cockpit

## Purpose

Target-class evidence is the hardware predicate that separates a retained
resource-cockpit conformance artifact from a high-scale performance claim. A
resource cockpit run may prove schema and runtime behavior on reduced RCH
workers, but any 64 CPU / 256 GiB or 200+ pane memory-envelope claim remains
`skipped_not_proven` until the run also retains a matching target-class
hardware artifact.

Target-class resource cockpit artifacts live at:

```text
tests/e2e/artifacts/target-class/<sku>/<run_id>/summary.json
```

The gate harness is:

```bash
FT_TARGET_CLASS_SKU=linux-x86_64-high-core scripts/run-target-class-cockpit.sh
FT_TARGET_CLASS_SKU=macos-apple-silicon-dev scripts/run-target-class-cockpit.sh
```

The wrapper records host facts first. If the selected SKU predicate is absent,
it writes a `skipped_not_proven` summary and does not run the reduced
conformance suite. If the predicate is present, it drives the existing
RCH-backed cockpit conformance lane in
`tests/e2e/test_ft_rz0eb_4_resource_cockpit_conformance.sh`.

## Major SKUs

| SKU | Major SKU | OS and kernel | CPU floor | Memory floor | Storage floor | Claim posture |
| --- | --- | --- | --- | --- | --- | --- |
| `macos-apple-silicon-dev` | macOS | macOS 15+, Darwin 24+, `arm64` Apple Silicon | 14 logical CPUs | 64 GiB | 50 GiB free on the repo volume | Operator-workstation cockpit smoke only; does not unlock 64 CPU / 256 GiB claims. |
| `linux-x86_64-high-core` | Linux | Linux `x86_64`, kernel 6+; Ubuntu 24.04 LTS or equivalent supported runner image | 64 logical CPUs | 256 GiB | 200 GiB free on the repo volume, preferably NVMe-backed | Required SKU for high-scale cockpit claims and 200+ pane memory-envelope wording. |

Full target-class proof requires both:

- a retained artifact under the SKU path above, with
  `hardware_predicate.proof_status = "proven_predicate_met"` and
  `evidence.high_scale_claim_allowed = true`;
- a retained cockpit conformance summary from the RCH-backed
  `ft-rz0eb.4` lane linked through `evidence.conformance_summary`.

Anything else is useful diagnostic evidence, but it is not release proof for
64 CPU / 256 GiB behavior.

## Current Artifact

The current checkout does not have target-class hardware available. The local
Mac has 14 logical CPUs and 64 GiB RAM, and the current RCH worker capability
set tops out at 10 logical CPUs. The retained gate artifact is therefore a
deliberate skip, not a proof:

```text
tests/e2e/artifacts/target-class/linux-x86_64-high-core/20260512T150000Z/summary.json
```

Expected interpretation:

- `status = "skipped_not_proven"`
- `hardware_predicate.target_class = false`
- `hardware_predicate.proof_status = "skipped_not_proven"`
- `evidence.high_scale_claim_allowed = false`

## Release Bundle Gate

Before release wording can cite the resource cockpit for a major SKU, the
release bundle must include at least one retained target-class summary for
that SKU. The minimum bundle set is one `macos` artifact and one `linux`
artifact. A `skipped_not_proven` artifact may document why the claim remains
blocked, but it must not satisfy high-scale release wording.

The G16 release-attestation bead owns final bundle materialization. This
contract is the input rule: every claimed major SKU needs a concrete artifact,
and high-scale wording needs the Linux high-core predicate to be proven.
