# Recorder Validation Gates (wa-oegrb.7.5)

This document defines recorder validation requirements. FrankenTerm uses DSR
for release orchestration and remote RCH for development Cargo proof; GitHub
Actions is prohibited by AGENTS.md. Historical workflow names are not evidence
that a gate is configured or passing.

## Scope

The gate bundles these validation surfaces:

1. Chaos/failure matrix (silent-loss prevention)
2. Recovery drills (checkpoint/reindex/writer crash paths)
3. Recorder correctness invariants
4. Semantic quality regression harness
5. Hybrid fusion correctness tests
6. Load harness check (`storage_regression` compile, with a separate measured run)

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

## Compile and measured modes

- Default mode: compile-checks the load harness and runs deterministic gates.
- Measured mode: sets `FT_RECORDER_GATE_RUN_LOAD_BENCH=1` to execute the benchmark.
  A successful compile is not a performance result.

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

## Development reproduction through RCH

The script delegates Cargo to RCH. Retain the remote worker identity and
transcripts, require nonzero named tests, and reject local fallback or an
unavailable worker as blocked proof. Never enable an Actions/local-Cargo bypass.

```bash
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 scripts/check_recorder_validation_gates.sh
```

Run measured mode through the same remote boundary:

```bash
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 \
  FT_RECORDER_GATE_RUN_LOAD_BENCH=1 scripts/check_recorder_validation_gates.sh
```

For an individual leg, pass the Cargo arguments below through the required
`RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec --`
prefix with a bounded job count, `--locked`, and a task-specific target directory.
These are argument lists, not local execution instructions:

```text
cargo test -p frankenterm-core --test recorder_tantivy_integration \
  chaos_failure_matrix_detects_faults_and_recovers_without_silent_loss -- --nocapture

cargo test -p frankenterm-core --test recorder_recovery_drills -- --nocapture
cargo test -p frankenterm-core --test recorder_correctness_integration -- --nocapture
cargo test -p frankenterm-core --test semantic_quality_harness_tests -- --nocapture
cargo test -p frankenterm-core --test hybrid_fusion_tests -- --nocapture

cargo bench -p frankenterm-core --bench storage_regression --no-run
```

## Release wiring and evidence

Use `scripts/release-gates.sh --list` and the configured DSR quality lane to
inspect current gate wiring. Record the exact source, enabled features, remote
or DSR host, test counts, artifact paths, and outcomes. This document alone does
not establish that recorder validation is included in a release or has passed.
