# Capture Scheduler Fairness SLO Contract

Date: 2026-05-10
Bead: `ft-n447z.1`
Status: current-behavior contract for downstream scheduler model, telemetry, policy, and proof beads

## Purpose

This contract pins the current polling capture scheduler semantics before any
fairness policy changes are made. It defines:

- the current scheduling algorithm in implementation terms;
- the fairness and starvation SLO names downstream tests must assert;
- the skipped-poll and deferral reason vocabulary future telemetry should use;
- which guarantees are strict correctness invariants versus benchmark-dependent
  targets.

This document is intentionally not a production behavior change. It is the
handoff contract for `ft-n447z.2` through `ft-n447z.6`.

## Code Anchors

| Surface | Anchor | Contract use |
| --- | --- | --- |
| Per-pane tailer state | `crates/frankenterm-core/src/tailer.rs::PaneTailer` | Poll interval, last poll, backpressure count, overflow-gap pending flag. |
| Scheduler | `crates/frankenterm-core/src/tailer.rs::CaptureScheduler` | Capture/byte budgets and priority tier selection. |
| Supervisor | `crates/frankenterm-core/src/tailer.rs::TailerSupervisor::spawn_ready` | Ready-pane filtering, equal-priority rotation, task admission, and poll outcome wiring. |
| Runtime wiring | `crates/frankenterm-core/src/runtime.rs::spawn_capture_task` | Config hot reload, pane priority computation, 10 ms spawn tick, and scheduler snapshot publishing. |
| Config | `crates/frankenterm-core/src/config.rs::IngestConfig` and `CaptureBudgetConfig` | `poll_interval_ms`, `min_poll_interval_ms`, `max_concurrent_captures`, and capture budgets. |
| Overflow policy | `docs/flight-recorder/capture-backpressure-overflow-policy.md` | Stable overflow GAP behavior and `backpressure_overflow` reason. |
| Timing doctrine | `docs/timing-determinism.md` | Adaptive polling and explicit GAP semantics. |

## Current Scheduling Algorithm

### Eligibility

A pane is eligible for polling only when all of these are true:

1. The pane is currently observed and has a synced tailer.
2. The pane's adaptive interval has elapsed.
3. The pane is not already in `capturing_panes`.
4. The supervisor is not shutting down.
5. The global byte budget is not exhausted.

If vendored streaming is active for a pane, that pane is excluded from polling
until streaming exits and runtime sync falls it back to polling.

### Adaptive Polling

Each pane owns an adaptive poll interval.

| Outcome class | Current effect |
| --- | --- |
| `Changed` | `last_poll = now`; interval resets to `min_interval`. |
| `OverflowGapEmitted` | `last_poll = now`; interval resets to `min_interval`; overflow state clears. |
| `NoChange` | `last_poll = now`; interval multiplies by `backoff_multiplier` up to `max_interval`. |
| `Backpressure` | `last_poll = now`; interval backs off; consecutive backpressure increments. |
| `NoCursor`, `ChannelClosed`, `CaptureTimeout`, `CircuitOpen`, `Error` | `last_poll = now`; interval backs off; specialized counters/logs update where present. |

Runtime maps config to tailer settings as follows:

| Runtime config | Tailer field |
| --- | --- |
| `ingest.min_poll_interval_ms` | `TailerConfig.min_interval` |
| `ingest.poll_interval_ms` | `TailerConfig.max_interval` |
| `ingest.max_concurrent_captures` | `TailerConfig.max_concurrent` |
| `ingest.budgets.max_captures_per_sec` | `CaptureBudgetConfig.max_captures_per_sec` |
| `ingest.budgets.max_bytes_per_sec` | `CaptureBudgetConfig.max_bytes_per_sec` |

The runtime currently attempts scheduler admission on a 10 ms spawn tick, while
tailer sync, priority recomputation, and snapshot publication happen on the
discovery interval.

### Priority Computation

Lower numeric priority means higher scheduler priority.

Runtime computes the effective priority for each observed pane from:

1. `PanePriorityConfig::priority_for_pane(domain, title, cwd)`;
2. a live pane priority override, if present and not expired.

Expired overrides are purged before priorities are handed to the supervisor.
Panes without explicit priority data default to `u32::MAX` inside the tailer.

### Ready Order

The supervisor orders ready panes by `(priority, pane_id)` for deterministic
input into the scheduler. Before selection, it rotates each equal-priority group
by that group's stored round-robin offset. After tasks are started, the offset
for each started priority advances by the number of started panes in that group.

This prevents stable pane-id tie-breaking from permanently favoring low pane
ids when equal-priority panes are continuously ready.

### Tier Selection

`CaptureScheduler::select_panes` receives priority-sorted ready panes plus the
number of available semaphore permits.

The effective selection limit is:

```text
min(available_permits, captures_remaining_this_second_or_unlimited)
```

Selection then splits ready panes into:

| Tier | Priority range | Contract meaning |
| --- | --- | --- |
| high | `0..=50` | Critical/high panes. |
| low | `51..=u32::MAX` | Normal/low/default panes. |

When `effective_limit >= 2`, the scheduler reserves a low-tier floor:

```text
target_low_count = max(1, effective_limit / 5)
guaranteed_low = min(low_ready_count, target_low_count)
```

High-tier panes take the remaining slots first. Unused high-tier capacity spills
back to the low tier. The selected count is debited from the per-second capture
budget immediately.

Important limitation: the floor protects the low tier as a whole. It does not
guarantee equal service across distinct low priority values such as `100` and
`200`. Equal-priority groups have round-robin protection; different priority
values still obey numeric priority order.

### Budgets

`0` means unlimited for both budget fields.

| Budget | Current behavior |
| --- | --- |
| `max_captures_per_sec` | Limits scheduled capture tasks per one-second scheduler window. Selection debits this budget before tasks run. |
| `max_bytes_per_sec` | Stops new scheduling once bytes remaining reaches zero. Bytes are debited after changed captures because the byte count is not known before capture. A single large capture can consume the remaining byte budget and saturate it to zero. |

The byte tracker is global for admission. The implementation also tracks
per-pane byte/capture windows for observability, but there is no per-pane byte
budget admission gate today.

### Backpressure And Overflow

The capture task reserves capacity on the downstream capture-event channel before
normal capture and before overflow GAP emission. If channel reservation exceeds
`send_timeout`, the poll outcome is `Backpressure`.

Backpressure semantics:

1. `consecutive_backpressure` increments for the pane.
2. At `OVERFLOW_BACKPRESSURE_THRESHOLD = 5`, `overflow_gap_pending = true`.
3. The next successful scheduling path emits a synthetic GAP instead of normal
   capture.
4. The GAP reason is the stable string `backpressure_overflow`.
5. After the GAP is emitted, `overflow_gap_pending` clears and the consecutive
   counter resets to zero.

This is a slow-and-signal policy. It does not promise lossless capture under
unbounded downstream congestion.

### Snapshot Surface

The current scheduler snapshot exposes:

- `budget_active`
- `max_captures_per_sec`
- `max_bytes_per_sec`
- `captures_remaining`
- `bytes_remaining`
- `total_rate_limited`
- `total_byte_budget_exceeded`
- `total_throttle_events`
- `tracked_panes`

It does not expose per-pane capture lag, skipped-poll reasons, selected/skipped
counts, starvation warnings, or tier-level fairness counters. Those missing
fields are the scope of `ft-n447z.3`.

## SLO Names

These names are the stable vocabulary for downstream tests, telemetry, docs, and
proof artifacts.

| SLO name | Type | Contract |
| --- | --- | --- |
| `capture.no_duplicate_inflight` | strict invariant | A pane already in `capturing_panes` must not receive a second poll task. |
| `capture.active_resets_to_min_interval` | strict invariant | A changed capture or emitted overflow GAP resets the pane interval to `min_interval`. |
| `capture.idle_backs_off_to_max_interval` | strict invariant | No-change and fail-closed poll outcomes back off by `backoff_multiplier` and never exceed `max_interval`. |
| `capture.equal_priority_rotation` | strict invariant | Under completed tasks, available permits, and non-exhausted budgets, equal-priority panes rotate so stable pane id order does not permanently starve higher pane ids. |
| `capture.low_tier_floor` | strict invariant | When high and low panes are ready and `effective_limit >= 2`, the low tier receives `min(low_ready_count, max(1, effective_limit / 5))` reserved slots before high-tier spillover; when multiple low-priority values are ready, the reserved floor rotates across the low tier so lower low-priority subtiers eventually receive service under sustained high-priority pressure. |
| `capture.single_slot_priority` | strict invariant | When `effective_limit == 1`, no low-tier floor is available; numeric priority order decides the single slot. |
| `capture.capture_budget_window` | strict invariant | `max_captures_per_sec > 0` limits scheduled captures inside the current one-second scheduler window. |
| `capture.byte_budget_stop` | strict invariant | Once global bytes remaining is zero, `spawn_ready` does not schedule new polling work until the byte window refills. |
| `capture.overflow_gap_after_backpressure_threshold` | strict invariant | Five consecutive backpressure outcomes mark the pane for an explicit `backpressure_overflow` GAP on the next successful scheduling path. |
| `capture.failure_is_explicit` | strict invariant | Timeout, channel-closed, circuit-open, no-cursor, and source-error outcomes must not be counted as successful captures. |
| `capture.service_lag_10_panes` | benchmark-dependent target | A reduced 10-pane model/proof should report max and p99 service-opportunity lag under nominal, budgeted, and backpressured cases. |
| `capture.service_lag_50_panes` | benchmark-dependent target | A 50-pane proof should report tier-level service shares, equal-priority rotation coverage, and starvation warnings. |
| `capture.service_lag_200_panes` | benchmark-dependent target | A 200-pane RCH proof should emit retained artifacts for lag histograms, skipped-poll reasons, scheduler snapshots, and source versus substrate failure classification. |
| `capture.service_lag_1000_panes_synthetic` | benchmark-dependent target | A synthetic 1000-pane model should prove bounded state, deterministic selection, and no silent starvation claims; it is not target-class hardware proof. |

Benchmark-dependent targets must be expressed as measured artifact rows, not
README-level support claims. A local or reduced RCH run can support reduced-scale
evidence only. It cannot prove 64+ CPU / 256 GiB target-class behavior.

## Skipped-Poll And Deferral Reason Vocabulary

These reason codes are the desired stable names for future telemetry. The
current code does not emit all of them yet.

| Reason code | Current source | Meaning |
| --- | --- | --- |
| `not_observed` | runtime/tailer sync | Pane is no longer in the observed pane set. |
| `streaming_mode` | runtime vendored streaming branch | Pane is receiving direct streaming deltas and is intentionally excluded from polling. |
| `streaming_fallback` | streaming task exit handling | Streaming ended and the pane is returning to polling. |
| `not_due` | `PaneTailer::should_poll` | Adaptive interval has not elapsed. |
| `already_capturing` | `capturing_panes` filter | A poll task is already in flight for the pane. |
| `global_capture_budget_exhausted` | `CaptureScheduler::select_panes` | Capture tokens are exhausted for the current scheduler window. |
| `global_byte_budget_exhausted` | `CaptureScheduler::is_byte_budget_exhausted` | Captured bytes remaining is zero for the current byte window. |
| `no_permit` | semaphore try-acquire fallback | Scheduler selected the pane but no semaphore permit could be acquired. |
| `send_backpressure` | `PollOutcome::Backpressure` | Capture channel reserve timed out before `send_timeout`. |
| `overflow_gap_pending` | `PaneTailer::overflow_gap_pending` | Next successful path must emit an overflow GAP before normal capture. |
| `overflow_gap_emitted` | `PollOutcome::OverflowGapEmitted` | Synthetic `backpressure_overflow` GAP was emitted. |
| `no_cursor` | `PollOutcome::NoCursor` | Pane has no cursor state at poll time. |
| `channel_closed` | `PollOutcome::ChannelClosed` | Capture-event channel closed while reserving capacity. |
| `capture_timeout` | `PollOutcome::CaptureTimeout` | `source.get_text` exceeded `capture_timeout`. |
| `capture_circuit_open` | `PollOutcome::CircuitOpen` | Capture circuit breaker rejected the request. |
| `capture_error` | `PollOutcome::Error` | Source returned an error that is not represented by a narrower reason. |
| `no_change` | `PollOutcome::NoChange` | Poll completed but produced no captured segment. |
| `changed` | `PollOutcome::Changed` | Poll completed and produced a captured segment. |
| `shutdown` | runtime shutdown flag | Supervisor stopped admitting polling work. |

Downstream telemetry should preserve these as machine-readable strings and avoid
embedding them only in prose logs.

## Required Test Matrix For `ft-n447z.2`

The deterministic model/test bead should cover at least:

| Scenario id | Required coverage |
| --- | --- |
| `scheduler_all_high_priority` | All ready panes in priority `0..=50`; selection preserves priority order and obeys permit/budget limits. |
| `scheduler_mixed_high_low_floor` | Mixed high/low panes with `effective_limit >= 2`; low-tier floor is asserted exactly. |
| `scheduler_single_slot_no_floor` | Mixed tiers with `effective_limit == 1`; no low-tier floor is asserted. |
| `scheduler_equal_priority_rotation` | Equal-priority groups rotate across repeated completed rounds. |
| `scheduler_low_subtier_starvation_probe` | Distinct low priorities prove the reserved low-tier floor rotates across lower low-priority subtiers while preserving high-tier precedence and single-slot priority semantics. |
| `scheduler_capture_budget_exhaustion` | Capture budget depletes across calls and refills only after the one-second window. |
| `scheduler_byte_budget_exhaustion` | Byte budget saturates to zero after capture and blocks new admission until refill. |
| `scheduler_backpressure_overflow_gap` | Five consecutive `send_backpressure` outcomes lead to `backpressure_overflow` on the next successful path. |
| `scheduler_priority_override_ttl` | Runtime-expired overrides do not affect effective priority; live overrides do. |
| `scheduler_10_50_200_1000_scale_rows` | Fixtures emit scale-tagged rows for 10, 50, 200, and synthetic 1000 panes. |

If a desirable SLO is not satisfied by current behavior, the model should record
that as a named expected gap and hand it to `ft-n447z.4`. It must not weaken this
contract silently.

## Required Telemetry Contract For `ft-n447z.3`

The telemetry bead should expose bounded data sufficient to answer:

1. Which panes are stale?
2. Why was each stale pane skipped or deferred?
3. How many selection opportunities did each priority tier receive?
4. Which budget or backpressure counter prevented work?
5. Is a starvation warning strict, benchmark-dependent, or not proven?

Minimum machine-readable fields:

| Field | Requirement |
| --- | --- |
| `pane_id` | Present for per-pane rows. |
| `priority` | Effective numeric priority at the time of the row. |
| `tier` | `high` for `0..=50`, `low` for `51..=u32::MAX`. |
| `mode` | `polling` or `streaming`. |
| `last_poll_age_ms` | Saturating age since the pane's last poll outcome, if known. |
| `current_interval_ms` | Current adaptive interval. |
| `last_reason_code` | One of the reason codes above. |
| `consecutive_backpressure` | Current consecutive backpressure count. |
| `overflow_gap_pending` | Whether a `backpressure_overflow` GAP is pending. |
| `starvation_warning` | `none`, `strict_invariant_failed`, `benchmark_target_missed`, or `not_proven`. |

Rows must be bounded under 200+ pane runs. If full per-pane output is too large
for a compact health surface, the compact snapshot should include top-N stale
panes plus aggregate counts, and the full artifact should be retained in the RCH
proof lane.

## Proof And Closeout Rules

For this contract bead:

- Static docs/prose checks are sufficient.
- No Cargo or RCH proof is required because no Rust code changed.
- Closeout must cite this file and list the SLO names downstream beads inherit.

For downstream implementation/proof beads:

- Any Rust tests, builds, benches, or E2E proof must run through `rch`.
- RCH sync, transfer, or worker selection logs are not source proof unless Cargo
  or the named harness actually starts.
- Reduced RCH proof is not target-class 64+ CPU / 256 GiB proof.
- Target-class claims require retained hardware predicate artifacts from the
  high-core runbook.

### `ft-n447z.5` Retained 200-Pane Proof Lane

The repeatable reduced-scale proof command is:

```bash
bash tests/e2e/test_ft_n447z_5_capture_fairness_200.sh
```

It runs the focused Rust proof through `run_rch_cargo_logged` with
`RCH_REQUIRE_REMOTE=1` and writes retained artifacts under:

```text
tests/e2e/artifacts/goal-line/ft-n447z.5/capture_fairness_200_pane_reduced/<run-id>/
```

The lane must emit `summary.json`, `proof-ledger.jsonl`, the raw RCH log and
metadata, and
`rust/capture_fairness_200_pane_summary.json`. The Rust summary is the source
artifact for pass/fail, lag histograms, skipped-poll reasons, poll outcomes,
and representative scheduler snapshots. The wrapper summary classifies any
failure as `source_or_test`, `environment`, or `rch_substrate`.

This lane is `remote_reduced` evidence only. It explicitly records
`target_class_hardware: "skipped_not_proven"` unless a separate high-core
hardware predicate artifact proves the 64+ CPU / 256 GiB target class.

## Non-Goals

- No scheduler policy change in this bead.
- No README support claim in this bead.
- No resource-cockpit v1 envelope change in this bead.
- No attempt to preserve every byte under unbounded overload.
- No second mux backend or capture engine.
