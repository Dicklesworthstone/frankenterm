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

The gate harness is `scripts/run-target-class-cockpit.sh`. It runs in three
modes (W9.4b):

```bash
# 1. Dry-run (default; safe on any host, e.g. the dev workstation). Exercises
#    host detection, the predicate preflight, and the artifact shape; retains
#    skipped_not_proven and exits 0. Produces no proof.
FT_TARGET_CLASS_SKU=linux-x86_64-high-core scripts/run-target-class-cockpit.sh

# 2. Preflight-only. Confirms a rented box meets the SKU floor BEFORE the long
#    paid run. Exits 0 only when the host is target-class eligible.
FT_TARGET_CLASS_SKU=linux-x86_64-high-core FT_TARGET_CLASS_PREFLIGHT_ONLY=1 \
  scripts/run-target-class-cockpit.sh

# 3. Run on the rented 64-CPU/256-GiB box. Fails fast with a clear message if
#    the host misses the floor; on a conforming high-scale host it runs the
#    W9.3 rehearsal, Criterion benchmark-budget lane, and cockpit conformance
#    lane before emitting a NON-skipped, signable summary.json.
FT_TARGET_CLASS_SKU=linux-x86_64-high-core FT_TARGET_CLASS_ALLOW_SKIP=0 \
  scripts/run-target-class-cockpit.sh
```

The wrapper records host facts first and always prints a human-readable
predicate preflight (observed vs required, per-check pass/fail, conforming
verdict). If the selected SKU predicate is absent it writes a
`skipped_not_proven` summary; with `FT_TARGET_CLASS_ALLOW_SKIP=0` it instead
fails fast and refuses to emit a non-proven artifact. If the predicate is
present it drives the existing W9.3 rehearsal, RCH-backed benchmark-budget
lane, and cockpit conformance lane:

- `scripts/high-scale-rehearsal.sh` for the bounded high-scale rehearsal
  receipt;
- `scripts/check_bench_budgets.sh` for the real Criterion budget gate;
- `tests/e2e/test_ft_rz0eb_4_resource_cockpit_conformance.sh` for schema and
  runtime cockpit conformance.

On a conforming run the emitted `summary.json` carries `ready_to_sign: true`
(set only when `status == "passed"` and
`hardware_predicate.proof_status == "proven_predicate_met"` and every required
lane reports `passed`), so the signing step is push-button. A skipped, failed,
or conformance-only artifact always reports `ready_to_sign: false`.

## Major SKUs

| SKU | Major SKU | OS and kernel | CPU floor | Memory floor | Storage floor | Claim posture |
| --- | --- | --- | --- | --- | --- | --- |
| `macos-apple-silicon-dev` | macOS | macOS 15+, Darwin 24+, `arm64` Apple Silicon | 14 logical CPUs | 64 GiB | 50 GiB free on the repo volume | Operator-workstation cockpit smoke only; does not unlock 64 CPU / 256 GiB claims. |
| `linux-x86_64-high-core` | Linux | Linux `x86_64`, kernel 6+; Ubuntu 24.04 LTS or equivalent supported runner image | 64 logical CPUs | 256 GiB | 200 GiB free on the repo volume, preferably NVMe-backed | Required SKU for high-scale cockpit claims and 200+ pane memory-envelope wording. |

Full target-class proof requires both:

- a retained artifact under the SKU path above, with
  `hardware_predicate.proof_status = "proven_predicate_met"` and
  `evidence.high_scale_claim_allowed = true`;
- a retained benchmark budget report in the artifact's `benches[]` block with
  `status = "passed"`;
- a retained W9.3 rehearsal summary in the artifact's `rehearsals[]` block with
  `status = "passed"`;
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
