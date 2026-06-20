# Round-6 B0 — Scored Hot-Frame Target List + A5 Quality Adjudication

> **The ≥0.5% attribution gate (bead `ft-p4vzl.1`).** No round-6 optimization
> idea earns a bead unless a profiler attributes ≥0.5% self-time to the frame it
> targets on a realistic workload. This document is the evidence. It is produced
> by two committed, fail-closed harnesses (cc_3):
> `crates/frankenterm-core/tests/round6_profile_realistic_workloads.rs` (B0) and
> `crates/frankenterm-core/tests/round6_quality_metric_harness.rs` (A5).
>
> **Proof:** `[RCH] remote vmi1227854 (1769.5s)` exit 0 — both targets passed,
> built `--profile release-perf` (opt-level 3, thin LTO). Compile also green on
> hz2 dev. Commits: `f84382411` (harnesses), `1568a9d74` (profile-robust gate).

## How to reproduce

```bash
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 CARGO_NET_GIT_FETCH_WITH_CLI=true \
  rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-p4vzl-harness \
  cargo test --profile release-perf -p frankenterm-core --no-fail-fast \
  --test round6_profile_realistic_workloads --test round6_quality_metric_harness -- --nocapture
```

The orchestrator can re-run on the quiet Mac for an Apple-Silicon datapoint; the
**share ranking and the gate verdict are relative and host-portable**, and the
A5 quality metrics are **deterministic** (identical on every host/profile).

---

## B0 — scored hot-frame target list

**Method.** Each frame is a leaf public call, so its warmed call-site mean ns
*is* its self-time. Per-op cost is measured in a tight loop; realistic share is
that cost weighted by a documented **fleet-minute call model** — a busy 64-pane
fleet over 60 s: `192 capture-deltas/s`, `64 outbound reads/s` (redaction),
`5 deep-scroll seeks/s`. Decoupling measurement from weighting keeps both
auditable: edit a model constant, re-run, the ranking re-derives.

**Run:** `release-perf` on vmi1227854 (x86_64 Linux), 2026-06-20.

| Rank | Frame | Location | mean ns/call | calls/min | self-time share | Gate (≥0.5%) |
|---|---|---|---:|---:|---:|---|
| 1 | `scan_pipeline.process` | `scan_pipeline.rs:528` | 9 818 | 11 520 | **72.47%** | ✅ eligible |
| 2 | `redactor.redact` | `redactor.rs:690` | 9 007 | 3 840 | **22.16%** | ✅ eligible |
| 3 | `scrollback.warm_line` | `scrollback_tiers.rs:883` | 26 973 | 300 | **5.18%** | ✅ eligible |
| 4 | `ingest.extract_delta` | `ingest.rs:1801` | 23.9 | 11 520 | 0.177% | ❌ below — no bead |
| 5 | `scrollback.locate_offset` | `scrollback_tiers.rs:1014` | 33.1 | 300 | 0.006% | ❌ below — no bead |

**Scan cost sensitivity (informational).** `scan_pipeline.process` mean:
representative-mixed = 9 818 ns, **ANSI-saturated = 47 812 ns (4.9×)**,
trigger-saturated = 9 747 ns (≈ mixed). The scan's cost is driven by **ANSI /
escape byte processing**, *not* trigger density — trigger-saturation barely
moves it.

### What the gate says about the round-6 candidate ideas

- **B1 — incremental cross-chunk Aho-Corasick (cc_1, FLAGSHIP) — CONFIRMED
  highest-EV.** It targets `scan_pipeline.process`, which is **72% of realistic
  hot-path self-time** — the single dominant frame by a wide margin. The gate is
  cleared overwhelmingly. *Caveat that sharpens the EV estimate:* the scan-stress
  numbers show per-call cost is dominated by ANSI byte processing, and
  trigger-saturated ≈ mixed, so the *trigger-scan* portion B1 touches is a minor
  share of the 72%. B1's actual lever is eliminating the **re-scan-at-flush
  double work** (README 1828-1830) — a structural win independent of per-call
  trigger cost — so it remains worth doing, but its ceiling should be sized
  against the double-scanned byte volume, not the full 72%.

- **redaction (round-5 "already-optimal") — 22% but leave it.** `redactor.redact`
  is #2 at 22%, but round-5 already measured its micro-levers (lookback) as
  sub-µs and rejected them. 22% of self-time in an *already-optimized* regex
  scan is not a new bead — it is the cost of the work. Do **not** re-open
  redaction micro-opts (pre-rejected class, round5-negative-results.md).

- **deep-scroll: optimize the DECODE (`warm_line`), not the LOCATE.**
  `scrollback.warm_line` (zstd page decode) is 27 µs/call and clears the gate at
  **5.18%**. `scrollback.locate_offset` is 33 ns/call and **0.006%** — three
  orders below the gate. **Implication for Q1 (prefix-index) / EV3 (blocked
  pages) / M4 (CDC dedup):** the realistic deep-scroll cost is the page
  *decompression*, so decode-path ideas (EV3 blocked single-line decode, M4
  shared-chunk reconstruction) target a gate-clearing frame, while a faster
  `locate_offset` (Q1's lever) is realistically irrelevant. *Depth caveat:* this
  harness uses 12 000 lines (~43 warm pages); Q1's reported 32× deep-scroll win
  is at far greater depth where the linear page-walk grows — even there a 32×
  speedup of a 33 ns op stays small in absolute terms unless depth is extreme.
  Q1 stays a latency-tail win at extreme depth, **not** a realistic-fleet CPU
  win.

- **B2 — Q3 KMP overlap on `extract_delta` (cc_2) — BELOW the gate (0.177%).**
  `ingest.extract_delta` is only **24 ns/call** in release; even at 11 520
  calls/min it is 0.177% of realistic self-time. The Q3 KMP optimization may win
  on adversarial repeated-first-byte input (its forced micro-bench), but it does
  **not** move the realistic-fleet needle. Per keep-gate rule #9 this frame does
  not clear the ≥0.5% gate, so B2 should be treated as a **correctness/robustness
  hardening of a worst-case path**, not a throughput win — and should not be
  promoted to default-on on perf grounds. (Retry predicate, Form 7: revisit only
  if a workload drives `extract_delta` overlap matching above 0.5% realistic
  self-time, e.g. pathological near-duplicate captures at very high pane count.)

**Bead-eligibility summary:** only `scan_pipeline.process`, `redactor.redact`,
and `scrollback.warm_line` clear the ≥0.5% gate. New round-6 ideas must target
one of these three frames (and not the already-optimal redaction lookback) to
earn a bead. `extract_delta` and `locate_offset` are below the gate.

---

## A5 — deterministic quality-metric adjudication

The two round-5 candidates that were measured on the wrong axis (compute, not
quality) are now adjudicated on their actual quality metric. Both metrics are
**deterministic** functions of the trace and policy — one run is the final,
host-independent verdict (no quiet host required; that is what makes them
*adjudicable*).

### S3-FIFO eviction (`cache.eviction=s3fifo`) — CONDITIONAL win

| Trace | metric | LFU (baseline) | S3-FIFO | rel Δ | Verdict |
|---|---|---:|---:|---:|---|
| scan-resistance (one-hit-wonder floods) | hit-rate @ cap 128 | 0.0901 | **0.1599** | **+77.4%** | ✅ WIN |
| phase-shift (migrating hot set) | hit-rate @ cap 128 | 0.7081 | 0.2533 | **−64.2%** | ❌ REGRESSION |

S3-FIFO **nearly doubles hit-rate** on the scan-heavy one-hit-wonder trace it was
built for (its purpose, finally captured) — but **collapses** when the working
set migrates (its small FIFO + ghost queue evicts newly-hot keys before they
prove out). **Verdict: workload-conditional, not a blanket default.** This
quantifies the round-5 instinct to keep it default-off and supplies the exact
promotion condition: enable only for confirmed scan-heavy, low-recency access
patterns. (Round-5 also measured S3-FIFO at ~2× LFU per-op compute; the +77%
scan-resistance hit-rate must be weighed against that compute cost for any
default-on decision — the conditional win is real but not free.)

### M9 PID fleet-memory dampening (`memory.dampening=pid`) — TIE (no captured benefit)

| metric | hysteresis (baseline) | PID | rel Δ | Verdict |
|---|---:|---:|---:|---|
| total evicted bytes @ pressure replay | 173 805 568 | 173 805 568 | 0.00% | TIE |
| reclaim-target oscillation (direction changes) | 0 | 0 | 0.00% | TIE |

On this 52-cycle, 192-pane sawtooth pressure replay, PID and hysteresis evict
**identical** bytes and **neither oscillates** (0 reclaim-target direction
changes for both). M9-PID's claimed quality benefit (fewer evicted bytes / less
oscillation) **does not manifest** on this workload — its round-5 compute cost
(−10%) buys no captured quality win, so there is no promotion case here.

*Harness limitation (honest):* the replay varies headroom in a sawtooth but the
`fleet_warm_bytes_target` trends monotonically down as memory is reclaimed, so
the oscillation metric stayed at 0 for both arms — the replay did not reach the
band-edge flip-flop regime where PID's damping would differ from hysteresis.
**Retry predicate (Form 7):** M9-PID stays default-off; revisit only if a
pressure replay that drives `fleet_warm_bytes_target` above-then-below a fixed
band (inducing ≥1 hysteresis direction change) shows PID with strictly fewer
direction changes and/or evicted bytes than hysteresis.

---

## Honesty notes

- ns are a single-host (vmi1227854 x86_64) `release-perf` datapoint — **not** an
  attested cross-engine perf claim. The deliverable is the **share ranking + gate
  verdict** (relative, host-portable) and the **deterministic A5 metrics**.
- The fleet-minute model constants are explicit modeling assumptions in the
  harness; the orchestrator can re-weight and re-derive.
- Q4 (lazy-captures suppression) and M2 (succinct-attr RSS) adjudicate via the
  same deterministic-scorecard shape and can be added to the A5 harness when
  their arms are wired; they are out of scope for `ft-...lw0s7.20` (M9 + S3-FIFO).
