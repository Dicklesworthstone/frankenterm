# Finish-Line Verification Contract (`ft-xbnl0.1.4`)

This document defines the shared evidence bar for the `ft-xbnl0` finish-line program.

It exists to answer one question in a way that survives chat history loss: what has to be true, and what proof has to exist on disk, before a finish-line bead may be closed?

## Normative Inputs

This contract builds on existing repo rules and lower-level specs instead of replacing them:

- `AGENTS.md`
- `docs/e2e-harness-spec.md`
- `docs/test-logging-contract.md`
- `docs/asupersync-migration-playbook.md`
- `docs/asupersync-runtime-doctrine.md`
- `tests/e2e/lib_rch_guards.sh`

If a lower-level spec conflicts with this document, follow the more specific rule unless it would weaken the finish-line evidence bar.

## Contract Invariants

Every non-planning finish-line bead must satisfy all of these:

1. Closure claims must be backed by artifacts on disk, not by chat memory.
2. Heavy compile, clippy, test, bench, or E2E verification must run through `rch exec -- ...` or through a harness that fails closed via `tests/e2e/lib_rch_guards.sh`.
3. A bead may close only if its proof includes exact commands, exact artifact paths, and enough logs to diagnose failure without rerunning from scratch.
4. If a lane narrows scope instead of implementing behavior, the code, docs, and tests must all reflect the narrower contract explicitly.
5. Consuming a dependency bead's evidence is allowed, but the closing note must cite the exact upstream artifact bundle instead of vaguely referring to earlier work.

## Evidence Levels

Every finish-line bead must declare which of these proof levels it requires and then satisfy them explicitly.

### Level A: Deterministic Local Surface Proof

Required for every implementation bead.

- Unit tests for happy path, edge cases, and failure modes.
- Integration or targeted E2E coverage for the touched surface.
- `cargo fmt --check`.
- `cargo check` and `cargo clippy -D warnings` for the touched crate or lane.

### Level B: Remote Verification Proof

Required for every finish-line bead that runs Cargo verification.

- Run the required Cargo commands via `rch exec -- ...` or a fail-closed harness.
- Use explicit target dirs such as `target/rch-<bead>-<purpose>`.
- Record the exact remote commands in the closing comment or artifact manifest.

### Level C: Artifact Bundle Proof

Required for every implementation, regression, soak, diagnostics, acceptance, or release-readiness bead.

Each run directory must contain, at minimum:

- `summary.json`
- `structured.log`
- raw per-step logs for every verification command

When applicable, also include:

- effective config snapshots
- exported diagnostics or query snapshots
- benchmark outputs
- trace bundles or leak samples
- redaction verification outputs

### Level D: Human/Operator Proof

Required only for operator-facing lanes such as docs, first-run setup, doctor/diagnostics, acceptance scenarios, and the final closure bundle.

- Evidence must show the operator path end to end, not just underlying unit coverage.
- The bundle must make it obvious how a future maintainer replays the acceptance path.

## Remote Execution Policy

The default command style for finish-line work is:

```bash
rch exec -- env CARGO_TARGET_DIR=target/rch-<bead>-<purpose> cargo test -p <crate> <filter> -- --nocapture
rch exec -- env CARGO_TARGET_DIR=target/rch-<bead>-<purpose> cargo check -p <crate> --all-targets
rch exec -- env CARGO_TARGET_DIR=target/rch-<bead>-<purpose> cargo clippy --no-deps -p <crate> --all-targets -- -D warnings
rch exec -- cargo fmt --check
```

If a lane needs a shell harness, it must fail closed on `rch` fallback and record the same commands inside its logs. Placeholder-remediation harnesses under `tests/e2e/` using `lib_rch_guards.sh` already satisfy this pattern and should be treated as the model.

Local Cargo verification is not sufficient for finish-line closure unless the bead explicitly documents why no remote verification applies.

## RCH Worker Parity Profile

Finish-line lanes that depend on remote Cargo verification must make the worker substrate itself auditable.

The minimum remote-verification profile is:

1. Capacity proof:
   - `rch workers probe --all --json` must show at least one reachable worker before any expensive step starts.
2. Remote smoke proof:
   - `tests/e2e/lib_rch_guards.sh` must either run its guarded `cargo check --help` smoke preflight or explicitly record `RCH_SKIP_SMOKE_PREFLIGHT=1` in the saved metadata and then fail closed on the first material remote Cargo step.
3. Metadata proof:
   - every `rch` log saved by `tests/e2e/lib_rch_guards.sh` must have a sibling `*.rch_meta.json`.
   - the metadata file must capture enough fields to audit the run without rereading the whole log:
     - selected worker identity when available
     - worker probe count or IDs when applicable
     - sync duration
     - remote command duration
     - remote exit code
     - wrapper exit code
     - whether smoke preflight was skipped
     - whether fail-open or timeout signals were detected
4. Artifact locality proof:
   - finish-line harnesses should call `rch_init "${ARTIFACT_DIR}" ...` so probe and smoke logs live inside the same per-run artifact bundle as the command logs and summary.
5. Topology drift proof:
   - if remote dependency-path parity is in doubt, use `tests/e2e/test_ft_124z4.sh` as the workspace-topology canary before trusting downstream remote evidence.

Representative remote command set for substrate validation:

```bash
rch exec -- env CARGO_TARGET_DIR=target/rch-<bead>-build cargo build -p frankenterm --bin ft
rch exec -- env CARGO_TARGET_DIR=target/rch-<bead>-test cargo test -p frankenterm-ssh match_exec -- --nocapture
rch exec -- env CARGO_TARGET_DIR=target/rch-<bead>-lint cargo clippy --no-deps -p frankenterm-ssh --all-targets -- -D warnings
rch exec -- cargo fmt --check
bash tests/e2e/test_ft_akx00_7_4_ssh_match_exec.sh
```

The direct commands prove build or test or lint or fmt against a known worker.
The shell harness proves that downstream fail-closed E2E wrappers keep the same audit surface instead of depending on chat memory.

## Artifact Contract

The finish-line program allows lane-specific artifact roots, but every per-run bundle must follow the same minimum shape:

```text
<lane-root>/<bead>/<scenario-or-surface>/<run_id>/
├── summary.json
├── structured.log
├── <step>.log
└── optional supporting files
```

Rules:

- `summary.json` must report overall pass or fail and the run directory.
- `structured.log` must be JSON-lines and include step start or pass or fail rows with a stable `correlation_id`.
- Raw step logs must preserve the exact command text and whether the backend was `rch`.
- Closing comments must cite absolute paths to the artifact directory, `summary.json`, and `structured.log`.
- Secrets must be redacted according to `docs/test-logging-contract.md`.

## Evidence Matrix

### Row 1: Async Cutover Closure

Applies to:
- `ft-xbnl0.2.1`
- `ft-xbnl0.2.2`
- `ft-xbnl0.2.3`
- `ft-xbnl0.2.4`
- `ft-xbnl0.2.5`
- `ft-xbnl0.2.6`

Required evidence:
- updated inventory or doctrine references when the migration surface changes
- deterministic unit or integration coverage for the touched async boundary
- remote `cargo test`, `cargo check`, `cargo clippy`, and `cargo fmt --check`
- audit proof that supported builds and docs do not reintroduce direct `tokio`, `smol`, or `async-io` usage where the lane claims closure

Failure diagnostics must identify:
- the exact async surface that still leaks legacy runtime usage
- whether the failure is semantic, compile-time, cancellation-related, or remote-execution-related

### Row 2: Supported-Path Capability Truth

Applies to:
- `ft-xbnl0.3.1`
- `ft-xbnl0.3.2`
- `ft-xbnl0.3.3`
- `ft-xbnl0.3.4`
- `ft-xbnl0.3.5`
- `ft-xbnl0.3.6`

Required evidence:
- unit tests covering the newly honest or fully implemented behavior
- targeted integration or E2E proof for the real operator or robot surface
- placeholder or fake-capability audits showing the removed string or branch is gone
- explicit citations to upstream `ft-akx00.*` artifact bundles when those beads provide prerequisite closure proof

Failure diagnostics must show:
- what user-visible capability is still fake, narrowed, or unsupported
- whether the lane implemented real behavior or changed the support contract honestly

### Row 3: Leak-Risk Inventory And Instrumentation

Applies to:
- `ft-xbnl0.4.1`

Required evidence:
- inventory artifact naming every tracked leak or retention class
- instrumentation proof showing the counters, spans, or diagnostics surfaces exist and are queryable
- remote verification for the supporting crates and tests

#### Leak-Risk Inventory Surface Map

`HealthSnapshot.leak_risk_inventory` is the canonical leak and retention substrate for `ft-xbnl0.4.1`.
Later beads in the lane should extend or consume this payload instead of creating parallel ad hoc probes.

| Signal | Source | What it means |
|--------|--------|---------------|
| `tracked_pane_entries`, `observed_pane_count` | `PaneRegistry` | How many pane lifecycles are still retained at all versus still actively observed by policy. |
| `window_count`, `tab_count`, `workspace_count` | `PaneRegistry` | Which higher-level mux containers are still reachable through tracked panes after churn, reconnect, or restore. |
| `pane_arena_count`, `pane_arena_tracked_bytes`, `pane_arena_peak_tracked_bytes` | `PaneRegistry::pane_arena_stats_snapshot()` | Live pane arena reservations and their retained-memory footprint; a count larger than live panes is a leak-risk signal. |
| `cursor_snapshot_bytes`, `cursor_snapshot_peak_bytes` | `RuntimeMetrics::lock_memory_snapshot()` | Current and peak cursor snapshot memory retained by runtime-side observation. |
| `storage_lock_contention_events`, `storage_lock_wait_max_ms`, `storage_lock_hold_max_ms` | `RuntimeMetrics::lock_memory_snapshot()` | Lock pressure that can pin buffers or hide retention behind stalled persistence. |
| `watchdog.overall`, `watchdog.unhealthy_components`, `watchdog.telemetry` | `HeartbeatRegistry::check_health()` and `HeartbeatRegistry::telemetry()` | Which runtime components are stalled badly enough to imply retained tasks, channels, or file descriptors. |

The inventory must stay queryable through all of these surfaces:

- crash-bundle `health_snapshot.json`
- `HealthSnapshotRenderer::render()` plain output
- `HealthSnapshotRenderer::render_compact()` summary output
- `HealthSnapshotRenderer::diagnostic_checks()` operator diagnostics

Failure diagnostics must identify the missing instrument, the missing lifecycle edge, or the unsupported probe path.

### Row 4: Leak Root-Cause And Remediation

Applies to:
- `ft-xbnl0.4.2`
- `ft-xbnl0.4.3`

Required evidence:
- deterministic repro or fixture proving the leak class before the fix
- deterministic regression proving the leak class no longer reproduces
- remote verification for the touched crates
- artifact bundles with leak snapshots, counts, or retained-object traces when relevant

Failure diagnostics must distinguish mux-server retention from runtime-side retention.

### Row 5: Deterministic Leak-Oracles

Applies to:
- `ft-xbnl0.4.4`

Required evidence:
- stable regression harnesses for churn, reconnect, shutdown, restore, and workflow storms
- structured per-step logs and clear failure signatures
- remote test and lint runs for the oracle harnesses themselves

Failure diagnostics must make it obvious which lifecycle scenario regressed and what metric crossed the allowed threshold.

### Row 6: Long-Haul Soak Proof

Applies to:
- `ft-xbnl0.4.5`

Required evidence:
- soak profiles for at least the blessed 50, 100, and 200-plus pane classes named by the bead
- run manifests with start time, duration, configuration, host identity, and outcome
- retained artifacts large enough to debug a mid-run failure without rerunning immediately

Failure diagnostics must capture:
- when the run failed
- which pressure or leak signal moved first
- whether the failure was in ft, the mux server, or the test harness substrate

### Row 7: Performance Budgets And Release Gates

Applies to:
- `ft-xbnl0.4.6`

Required evidence:
- explicit SLO or budget numbers written into the lane artifacts
- benchmark or measured workload outputs stored in the run bundle
- gating logic that fails when the budget is exceeded

Canonical release-gate entrypoints for this row:
- policy contract: `docs/ft-xbnl0-4-6-release-gates.json`
- deterministic gate harness: `tests/e2e/test_ft_xbnl0_4_6_release_gates.sh`

Release-readiness beads should consume those artifacts directly instead of restating private threshold tables in their own notes.

Failure diagnostics must report the exact metric, threshold, observed value, and recommended next inspection path.

### Row 8: Docs, Guards, And Operator Playbooks

Applies to:
- `ft-xbnl0.5.1`
- `ft-xbnl0.5.2`
- `ft-xbnl0.5.3`

Required evidence:
- docs diffs showing the final native architecture and honest support matrix
- permanent guards or audits exercised in CI-friendly or doctor-friendly form
- exact remote commands or non-Cargo checks used to validate the guardrails

Canonical entrypoints for this row:
- `docs/ft-xbnl0-5-3-blessed-tuning-playbook.md`
- `scripts/check_ft_xbnl0_5_3_blessed_tuning_profiles.sh`
- `tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh`

Failure diagnostics must show where docs still describe migration-era behavior or where a guard fails to catch a known forbidden pattern.

### Row 9: Operator Diagnostics And First-Run Recovery

Applies to:
- `ft-xbnl0.5.6`
- `ft-xbnl0.5.7`

Required evidence:
- doctor, status, or robot outputs captured from real runs
- first-run or recovery scenario artifacts showing the user path end to end
- diagnostics bundles that point to actionable next steps rather than generic failure text

Failure diagnostics must keep the operator path reproducible from the saved artifacts alone.

### Row 10: Operator Acceptance Scenarios

Applies to:
- `ft-xbnl0.5.4`

Required evidence:
- fresh-install through long-haul recovery scenarios executed against the final supported story
- exact commands, environment assumptions, and artifact bundle paths
- clear pass or fail criteria for each operator scenario

Failure diagnostics must say which acceptance checkpoint failed and what evidence disproved the release claim.

### Row 11: Final Closure Bundle

Applies to:
- `ft-xbnl0.5.5`

Required evidence:
- index of every finish-line row and the bead or artifact bundle that satisfied it
- remaining-risk register and any explicitly deferred work
- release-candidate checklist with exact commands, artifact locations, and operator pathways

This row may not invent new proof. It only closes when the earlier rows are already satisfied and indexed cleanly.

## Finish-Line Release Bar

The `ft-xbnl0` program is not over the goal line until all of the following are true:

1. Every open child lane under `ft-xbnl0` has either closed or been explicitly superseded in bead-native form.
2. Every closed non-planning lane cites exact `rch` commands or fail-closed harness commands and exact artifact locations.
3. No supported-path fake capability remains without an explicit narrowed contract and regression guard.
4. Leak, soak, and performance claims have stored evidence rather than anecdotal local observations.
5. README, AGENTS, and operator docs match the final support story.
6. `ft-xbnl0.5.5` can index the whole campaign without depending on chat transcripts.

## Closure Comment Template

Every finish-line closeout should be able to answer this template directly:

```text
Bead: <id>
Claim: <what is now true>
Exact commands:
- rch exec -- ...
- bash tests/e2e/...
Artifacts:
- <absolute artifact dir>
- <absolute artifact dir>/summary.json
- <absolute artifact dir>/structured.log
Consumed upstream evidence:
- <bead id> -> <absolute artifact path>
Residual risks:
- none | <explicit remaining risk>
```

If a closeout cannot fill this template, it is not ready to close.
