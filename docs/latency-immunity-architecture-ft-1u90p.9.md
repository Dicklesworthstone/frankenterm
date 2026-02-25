# Latency-Immunity Architecture Contract (`ft-1u90p.9`)

Date: 2026-02-22  
Status: Closure-audited (2026-02-25)  
Parent: `ft-1u90p`  
Related: `ft-3kxe`, `ft-e34d9`, `ft-brc7d`, `ft-283h4`

## Mission

Guarantee that FrankenTerm stays locally responsive for:
- text entry
- resize / reflow
- font-size changes

even when remote mux servers are degraded (high CPU, swap pressure, disk/IO stalls, protocol lag).

This is a **hard architecture contract**, not a best-effort tuning note.

## Closure Audit (2026-02-25)

Audit owner: `MagentaDesert`  
Bead: `ft-1u90p.9`

Summary:
- All child execution tracks are closed: `ft-1u90p.9.1` through `ft-1u90p.9.7`.
- Child granular beads under each track are closed.
- This parent contract remains the authoritative architecture reference and can be treated as complete for track-level delivery.

Evidence anchors:
- SLO baseline and thresholds: `docs/resize-performance-slos.md`
- Backpressure guardrail e2e harness: `tests/e2e/rio/test_input_preserving_backpressure_guardrails.sh`
- Controlled beta loop contract and checkpoint artifacts:
  - `docs/resize-controlled-beta-feedback-loop-wa-1u90p.8.7.md`
  - `evidence/wa-1u90p.8.7/cohort_daily_summary.json`
  - `evidence/wa-1u90p.8.7/decision_checkpoint_20260222.md`

Residual gate note:
- `ft-1u90p.8.7` (controlled beta feedback loop) remains `in_progress` with current checkpoint `HOLD`.
- That rollout-confidence gate is tracked in the rollout track (`ft-1u90p.8.*`) and does not reopen this architecture contract.

## Incident Archetype (What We Must Be Immune To)

Observed legacy failure mode:
- remote mux server sustained high CPU (`~41%`)
- swap pressure (`~882MB` swapped out)
- storage pressure (`~33.5MB` allocatable chunks left)
- dozens of active agents / PTYs produce output bursts
- mux falls behind, backlog amplifies, interactive UX degrades

User-visible failures:
- keystroke latency spikes
- resize hitching
- stale/stretchy intermediate frames during reflow

## Architecture Thesis

Split the runtime into two strict planes with one-way pressure isolation:

1. **Local Interaction Plane (LIP)**  
   Owns input admission, resize intent scheduling, viewport-priority reflow, and presentation deadlines.

2. **Remote State Plane (RSP)**  
   Owns remote mux ingest, remote pane synchronization, and best-effort convergence.

The RSP is allowed to degrade. The LIP is not.

## Existing Hooks We Reuse

- `crates/frankenterm-core/src/resize_scheduler.rs`
  - interactive/background classes
  - input guardrail budgets
  - domain-aware scheduling hooks
- `crates/frankenterm-core/src/viewport_reflow_planner.rs`
  - viewport-first + overscan + cold-scrollback batching
- `crates/frankenterm-core/src/backpressure_severity.rs`
  - continuous severity and proportional throttling
- `crates/frankenterm-core/src/wezterm.rs`
  - mux-pool path + CLI fallback in send path
- `docs/resize-performance-slos.md`
  - current contract for M1/M2/M3/M4 thresholds

## Non-Negotiable Invariants

| ID | Invariant | Budget / Rule |
|---|---|---|
| `LI-1` | Local input path cannot block on remote RPC completion | `0` synchronous waits in keypress fast path |
| `LI-2` | Viewport reflow always outranks remote catch-up work | `viewport_core` must preempt all background work |
| `LI-3` | Remote-domain storms cannot consume local interaction reserve | Hard reserve floor for local-interaction units per frame |
| `LI-4` | Under pressure, degrade quality before interactivity | Drop/defer cold scrollback before touching input/viewport |
| `LI-5` | If truth is uncertain, surface explicit uncertainty markers | GAP / stale markers, never silent corruption |

## SLO Addendum (Latency Immunity)

These supplement `docs/resize-performance-slos.md`:

- Local keystroke-to-echo (interactive panes):  
  - p95 `<= 10ms`
  - p99 `<= 20ms`
- Resize intent to first coherent viewport frame:  
  - p95 `<= 20ms` (mid tier)
  - p99 `<= 33ms` (mid tier)
- During remote mux outage/degradation windows:
  - no freeze > `100ms` on local text entry
  - no critical artifact incidence (`0` hard artifacts)

## Recommendation Contracts (EV-Gated)

EV formula:
`EV = (Impact * Confidence * Reuse) / (Effort * AdoptionFriction)`

Only recommendations with `EV >= 2.0` are implementation candidates.

### Card A: Local Snapshot Fast-Path with Seqlock + Epoch Reclamation

- Change:
  - Introduce a local snapshot state for interaction-critical pane metadata using versioned optimistic reads (seqlock) and epoch-based deferred reclamation for old snapshots.
- Hotspot evidence:
  - resize/input path must not block on remote state churn; current remote/mux uncertainty appears in send/adapter paths.
- Mapped graveyard sections:
  - `§14.9` Seqlocks
  - `§14.10` EBR
- EV score:
  - Impact `5`, Confidence `4`, Reuse `5`, Effort `3`, Friction `2` -> `EV=3.33`
- Priority tier:
  - `S`
- Adoption wedge:
  - start in read-only snapshot path for `ft status` and resize scheduler decision inputs
- Budgeted mode:
  - max snapshot versions in memory per pane; force compaction under pressure
- Expected-loss model:
  - States: `{fresh, stale, remote_lagged}`
  - Actions: `{serve_local_snapshot, block_for_remote, degrade_to_marker}`
  - Loss: blocking local interaction has highest loss; stale-but-marked snapshot has lower loss
- Fallback trigger:
  - repeated snapshot validation failures or livelock retries > threshold
- Proof artifacts:
  - loom/proptest for no torn-visible state + no UAF invariants
  - golden equivalence traces for visible behavior
- Rollout:
  - feature flag `latency_immunity_snapshot_fastpath`

### Card B: Remote Health Isolation with SWIM-Style Suspicion + Sequential Declaration

- Change:
  - Use probabilistic remote health state (`alive/suspect/degraded/dead`) to gate RSP work without penalizing LIP.
- Hotspot evidence:
  - remote stalls and false-positive failure assumptions amplify interaction latency.
- Mapped graveyard sections:
  - `§13.7` SWIM-style membership/failure detection
  - `§13.13` LDFI (targeted fault campaigns)
- EV score:
  - Impact `4`, Confidence `4`, Reuse `4`, Effort `2`, Friction `2` -> `EV=4.0`
- Priority tier:
  - `S`
- Adoption wedge:
  - start as read-only health classifier feeding scheduler domain budgets
- Budgeted mode:
  - bounded probe cadence, bounded suspicion memory, bounded retry budget
- Expected-loss model:
  - States: `{healthy, flaky, failed}`
  - Actions: `{normal_budget, isolate_remote_domain, quarantine_remote_domain}`
  - Loss: false isolation < allowing remote collapse to starve local interactivity
- Fallback trigger:
  - calibration drift or false-positive budget breach
- Proof artifacts:
  - deterministic replay for suspected->dead transitions
  - fault-injection reports proving local-interaction SLO preservation
- Rollout:
  - feature flag `latency_immunity_remote_isolation`

### Card C: Input-Preserving Backpressure with S3-FIFO and Strict Reserve Floors

- Change:
  - pair continuous severity throttling with cache/queue admission that protects interaction-critical working sets and sheds cold data first.
- Hotspot evidence:
  - queue depth escalation is already modeled, but local interaction reserve needs hard floor semantics under all tiers.
- Mapped graveyard sections:
  - `§15.1` S3-FIFO
  - `§0.15` tail-latency decomposition requirement
  - `§0.19` evidence-ledger schema
- EV score:
  - Impact `4`, Confidence `4`, Reuse `5`, Effort `2`, Friction `2` -> `EV=5.0`
- Priority tier:
  - `S`
- Adoption wedge:
  - apply first to reflow/capture queue admission and cold-scrollback cache
- Budgeted mode:
  - hard minimum interaction reserve units per frame
  - bounded queue ages with explicit stale/drop markers
- Expected-loss model:
  - States: `{green, yellow, red, black}`
  - Actions: `{normal, throttle_background, drop_cold, isolate_remote}`
  - Loss: dropping cold history << dropping/lagging interactive work
- Fallback trigger:
  - any interaction-SLO breach while in non-black tier
- Proof artifacts:
  - before/after queue-age histograms
  - replay traces showing no interaction starvation
- Rollout:
  - feature flag `latency_immunity_input_reserve_floor`

### Card D: Linux io_uring Data-Plane Fast Path (Conditional)

- Change:
  - optional Linux-only io_uring submission/completion batching for high-volume mux I/O.
- Hotspot evidence:
  - syscall overhead under sustained PTY fanout.
- Mapped graveyard sections:
  - `§15.8` io_uring
- EV score:
  - Impact `4`, Confidence `3`, Reuse `4`, Effort `3`, Friction `2` -> `EV=2.67`
- Priority tier:
  - `A` (after Cards A-C)
- Adoption wedge:
  - vendored mux path only; explicit fallback to existing path
- Budgeted mode:
  - max in-flight SQEs and registered buffer pool caps
- Expected-loss model:
  - prefer io_uring only when calibration proves lower tail latency and error budgets remain clean
- Fallback trigger:
  - kernel incompatibility, CQE error-rate spike, or calibration mismatch
- Proof artifacts:
  - isomorphism tests across backends + sustained soak
- Rollout:
  - feature flag `io_uring_mux_fastpath`

## First Implementation Sequence

1. Card C first (input reserve hardening + queue policy):
   - least invasive, immediate UX protection
2. Card A next (local snapshot fast path):
   - structural isolation of interaction read paths
3. Card B next (remote domain health isolation):
   - remove remote-failure coupling from local interaction
4. Card D optional follow-on (Linux acceleration only):
   - after invariants and SLO stability are proven

## Validation and Evidence Requirements

- Unit + integration + e2e + soak + fault-injection
- Required telemetry fields:
  - input latency p95/p99
  - resize stage p95/p99
  - queue age/depth by domain
  - stale/drop marker counts
  - remote health classifier state transitions
- Required artifacts:
  - baseline + post-change performance bundles
  - replayable failure traces
  - decision checkpoints with explicit GO/HOLD/ROLLBACK

## Compute Policy (Mandatory)

All CPU-intensive checks for this track must use `rch`:

```bash
rch exec -- cargo check --workspace --all-targets
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
rch exec -- cargo test --workspace
```

## Rollback and Safety

Every card is feature-flagged and independently reversible.  
Any breach of `LI-1..LI-5` or latency-immunity SLO addendum forces immediate rollback to prior stable path.
