# Recorder Validation Gates (wa-oegrb.7.5)

This document defines the CI/nightly quality gates for the recorder validation track.

## Scope

The gate bundles these validation surfaces:

1. Chaos/failure matrix (silent-loss prevention)
2. Recovery drills (checkpoint/reindex/writer crash paths)
3. Recorder correctness invariants
4. Semantic quality regression harness
5. Hybrid fusion correctness tests
6. Load harness check (`storage_regression` bench compile in CI, optional run in nightly)

## Gate Entrypoint

The canonical gate script is:

```bash
scripts/check_recorder_validation_gates.sh
```

Artifacts are written to:

```text
target/recorder-validation-gates/
```

Primary report:

```text
target/recorder-validation-gates/recorder-validation-report.json
```

## CI vs Nightly Modes

- CI mode (default): compile-checks load harness (`cargo bench --no-run`) and enforces deterministic test gates.
- Nightly mode: sets `FT_RECORDER_GATE_RUN_LOAD_BENCH=1` to run the recorder swarm-load benchmark path.

Environment toggles:

```bash
FT_RECORDER_GATE_RUN_LOAD_BENCH=1                   # enable bench execution
FT_RECORDER_VALIDATION_ARTIFACT_DIR=custom/path     # optional artifact dir override
FT_RECORDER_VALIDATION_TARGET_DIR=custom/target     # optional CARGO_TARGET_DIR override
```

## Explicit Threshold Policy

The script enforces:

1. At least `1` chaos matrix summary artifact:
   - `[ARTIFACT][recorder-chaos] matrix_summary=...`
2. At least `3` recovery drill artifacts:
   - `[ARTIFACT][recorder-recovery-drill] ...`
3. At least `10` correctness invariant tests executed in `recorder_correctness_integration`.

Any threshold miss fails the job.

## Local Reproduction

Run the exact CI gate locally:

```bash
scripts/check_recorder_validation_gates.sh
```

Run full nightly-equivalent mode locally:

```bash
FT_RECORDER_GATE_RUN_LOAD_BENCH=1 scripts/check_recorder_validation_gates.sh
```

Targeted repro commands (individual legs):

```bash
cargo test -p frankenterm-core --test recorder_tantivy_integration \
  chaos_failure_matrix_detects_faults_and_recovers_without_silent_loss -- --nocapture

cargo test -p frankenterm-core --test recorder_recovery_drills -- --nocapture
cargo test -p frankenterm-core --test recorder_correctness_integration -- --nocapture
cargo test -p frankenterm-core --test semantic_quality_harness_tests -- --nocapture
cargo test -p frankenterm-core --test hybrid_fusion_tests -- --nocapture

cargo bench -p frankenterm-core --bench storage_regression --no-run
```

## Workflow Wiring

- PR/push gate job: `.github/workflows/ci.yml` (`recorder-validation-gates`)
- Scheduled nightly gate: `.github/workflows/nightly-recorder-validation.yml`
