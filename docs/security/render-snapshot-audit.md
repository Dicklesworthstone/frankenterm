# Render-Snapshot Migration Audit

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.3.2.cont] / `ft-q6x91`
**Status:** Foundation slice shipped — static-audit
predicate + call-site registry + lock-wait probe contract
+ JSONL log row + migration-readiness rubric + 26 lib
tests. Integration follow-on: paint.rs editing, actual
`Instant::now()` probe insertion, E2E heavy-burst test,
visual-regression test.

The render-snapshot substrate already lives at
`render_snapshot_guard.rs` (`25e095d10`, 31 tests). This
module ships the **integration substrate** the bead's
continuation work consumes — particularly the static-
audit invariant that catches misclassifications at
compile-time-equivalent.

## Headline rule

> **Mutation paths (input thread, PTY-driven dirty
> events) STILL go through writer side. Render thread
> NEVER touches `SnapshotKind::Mutation`.**
>
> The audit registry rejects misclassifications: a
> render-thread call site that acquires `Mutation` →
> `MutationFromForbiddenClass` violation.

## Static-audit invariant (sub-task 6)

`CallPathClass`: closed list of 4.

| Class | `may_acquire_mutation_kind()` |
|---|---|
| `RenderThread` | **false** |
| `InputThread` | **true** |
| `A11yThread` | false |
| `DirtyLineReader` | false |

Per-class rule pinned by tests:
`render_thread_class_cannot_acquire_mutation`,
`input_thread_class_can_acquire_mutation`,
`a11y_thread_class_cannot_acquire_mutation`,
`dirty_line_reader_class_cannot_acquire_mutation`.

`CallSite::validate()` returns `MutationFromForbiddenClass`
violation if the site's `acquires_mutation` field
contradicts its class.

## Call-site registry (sub-task 1)

`CallSiteRegistry` with three audit checks:

1. **MutationFromForbiddenClass** — the invariant rule.
2. **MissingInstrumentation** — every render-thread site
   must be marked instrumented (sub-task 5 wired).
3. **DuplicateSiteId** — same id registered twice (likely
   a refactor leaving stale entries).

`registry.audit() -> Vec<AuditViolation>` returns the
violation list (empty `Vec` on success).
`count_by_class()` returns per-class totals for the
doctor surface.

## Lock-wait probe (sub-task 5)

Typed-state `LockAcquireProbe<Stage>`:

```rust
let probe = LockAcquireProbe::<Pending>::before(site_hash, before_ns);
// ... acquire lock ...
let recorded = probe.record_after(after_ns);
let delta = recorded.delta_ns(); // saturates to 0 on clock skew
```

The integration projects `delta_ns` onto the substrate's
`LockWaitDistribution::record(ns)`.

`probe_clock_skew_saturates_to_zero` test pins the safety
behavior under non-monotonic clocks.

## Migration-readiness rubric

`evaluate_migration_readiness(inputs) ->
MigrationReadinessVerdict`:

5-gate decision tree, evaluated in order:

1. `registry_audit_violations > 0` → `AuditViolations`
2. instrumented < total → `PartialInstrumentation`
3. `lock_wait_p99_meets_target == false` → `P99TargetMissed`
4. `e2e_heavy_burst_passing == false` → `HeavyBurstFailing`
5. `visual_regression_passing == false` →
   `VisualRegressionFailing`
6. all pass → `Ready`

Each blocker has an explicit slug for the doctor + log.

## JSON-line log (sub-task 7)

`StructuredLogRow` (tagged):

- `FrameAcquire { ts_ns, frame_idx, lock_wait_ns,
  snapshot_kind_slug, classification_slug }` — per-frame.
- `FrameEnd { ts_ns, frame_idx, render_ns, over_budget }` —
  per-frame end.
- `SessionSummary { total_frames, p50/p95/p99_ns,
  meets_p99_target }` — per-session.

Bidirectionally clean via `render_log_jsonl` /
`parse_log_jsonl`.

## Health snapshot

`RenderSnapshotAuditHealth`:

- `registered_sites_total` / `instrumented_sites_total`.
- `audit_violations_total` — lifetime counter.
- `last_verdict` — slug of the most recent rubric output.
- `sites_by_class` / `violations_by_kind` — histograms.
- `is_safe()`: zero audit violations AND last verdict was
  `ready`.

`record_audit_run(registry)` runs the audit + projects
counts; `record_verdict(verdict)` records the rubric
outcome. The doctor only marks safe after both have run
cleanly.

## "DO NOT BREAK" rules

- **Mutation paths still go through writer side** —
  `CallPathClass::InputThread.may_acquire_mutation_kind()
  == true`; non-`InputThread` classes return `false`.
- **A11Y tree updates see consistent snapshots** —
  `A11yThread` class cannot acquire `Mutation` guards.
- **Per-line dirty reads dirty-set from snapshot** —
  `DirtyLineReader` class cannot acquire `Mutation`
  guards.

## Tests (26)

- 4 per-class invariant tests.
- 4 site-validation tests (well-formed render, render-
  acquires-mutation rejected, input acquires-mutation OK,
  count-by-class).
- 4 registry audit tests (catch misclassification,
  catch missing instrumentation, catch duplicate id,
  clean-when-well-formed).
- 2 lock-wait probe tests.
- 1 JSONL roundtrip.
- 6 readiness rubric tests (Ready + 5 blockers).
- 3 health-snapshot tests.
- 1 headline scenario:
  `migration_complete_scenario`.
- 1 registry serde roundtrip.

## Bead acceptance status

| Sub-task | Status |
|---|---|
| 1 — Audit grep paint.rs RwLock<TerminalState>::read() | ⏳ integration script (foundation: `CallSiteRegistry` shape) |
| 2 — Replace with triple_buffer.read() | ⏳ paint.rs editing |
| 3 — Cell access through SnapshotGuard | ⏳ paint.rs editing |
| 4 — Frame-end drop guard | ⏳ paint.rs editing |
| 5 — Lock-wait instrumentation | ✓ `LockAcquireProbe` typed-state contract |
| 6 — Static-analysis audit | ✓ `CallSiteRegistry::audit()` + 3-violation taxonomy |
| 7 — JSON-line per-frame logs | ✓ `StructuredLogRow` |
| 8 — E2E heavy-burst | ⏳ test integration |
| 9 — Visual regression | ⏳ test integration |
| Migration-readiness rubric | ✓ `evaluate_migration_readiness` |
| Per-release attestation | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- Substrate: `render_snapshot_guard.rs` (`SnapshotKind`,
  `LockWaitDistribution`, `meets_p99_target`).
- Sibling: `ft-2okh0.3.1` (TripleBuffer abstraction),
  `ft-2okh0.3.4` (Loom proof of TripleBuffer correctness),
  `frame_budget_signal_coupling` (this run — render-thread
  budget composition).
- Attestation: `ft-syqcz.1`.
