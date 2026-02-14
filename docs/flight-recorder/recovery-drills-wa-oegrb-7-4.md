# Recorder Recovery Drills (`wa-oegrb.7.4`)

This document defines repeatable operator drills for checkpoint/replay/reindex recovery.

## Prerequisites
- Build/test environment can run `cargo test -p frankenterm-core`.
- Recorder append-log test fixtures can be created in temp directories.
- Team captures test stdout (`--nocapture`) so artifacts are retained.

## Drill Scenarios

### 1. Writer Crash Before Commit
Command:
```bash
CARGO_TARGET_DIR=target-recovery-drills \
  cargo test -p frankenterm-core --test recorder_recovery_drills \
  recovery_drill_writer_crash_resume_replays_without_loss -- --nocapture
```

Success criteria:
- Initial run fails with commit failure.
- Checkpoint does not advance on failed commit.
- Resume run replays full event set with zero lag.
- Artifact emitted: `[ARTIFACT][recorder-recovery-drill] writer_crash_resume=...`

### 2. Checkpoint Divergence Detection
Command:
```bash
CARGO_TARGET_DIR=target-recovery-drills \
  cargo test -p frankenterm-core --test recorder_recovery_drills \
  recovery_drill_checkpoint_divergence_detected_then_resumed -- --nocapture
```

Success criteria:
- Regression checkpoint write is rejected (`CheckpointRegression`).
- Resume run catches up remaining events.
- Final lag is zero.
- Artifact emitted: `[ARTIFACT][recorder-recovery-drill] checkpoint_divergence=...`

### 3. Reindex Resume + Integrity Verification
Command:
```bash
CARGO_TARGET_DIR=target-recovery-drills \
  cargo test -p frankenterm-core --test recorder_recovery_drills \
  recovery_drill_reindex_resume_integrity_consistent -- --nocapture
```

Success criteria:
- First reindex run stops early (`max_batches=1`).
- Resume run continues from checkpoint without clearing prior progress.
- Combined output passes integrity check against append-log.
- Artifact emitted: `[ARTIFACT][recorder-recovery-drill] reindex_resume_integrity=...`

## Recovery Timing Tracking

Each drill records `recovery_ms` in emitted artifacts. Track these values by run date:
- baseline run
- latest run
- delta %

Escalate if any `recovery_ms` regresses by >20% without an approved rationale.

## Post-Drill Report Template

Use this template after each rehearsal:

```md
## Recovery Drill Report
- Date:
- Operator:
- Branch/commit:
- Drill(s) run:

### Outcomes
- Writer crash resume:
- Checkpoint divergence:
- Reindex resume integrity:

### Metrics
- recovery_ms (writer crash):
- recovery_ms (checkpoint divergence):
- recovery_ms (reindex resume):

### Artifacts
- stdout/log links:
- captured `[ARTIFACT][recorder-recovery-drill]` lines:

### Gaps / Follow-ups
- Observed failure modes:
- Backlog items created/updated:
- Owner + due date:
```
