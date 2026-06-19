# Round-4 Keep Ledger

> The Alien Optimization Gauntlet (v0.7.0 campaign). Every *kept* optimization gets an entry here with its
> same-run-window A/B proof, behavior-preservation proof, and rollback recipe. Rejected/reverted candidates →
> [`round4-negative-results.md`](round4-negative-results.md). Keep-gate rules + retry vocabulary documented there.

Campaign record: [`../../tests/artifacts/perf/v070-round4-campaign.md`](../../tests/artifacts/perf/v070-round4-campaign.md).

---

## Keep entry template (copy per kept change)

```markdown
### <YYYY-MM-DD> | <bead_id> | <Title>

**Status:** kept (durable optimization | durable infra | structural)

**Gate:** <feature/env/config flag> + default state (off until proven; promotion note if flipped on)

**Profile attribution:** "Closed <X>% <Frame> self-time" — flamegraph: <path>

**Measurement (focused):** <bench> <metric> = <before> → <after> (<delta%>, <speedup>); cv_pct=<X> (≤5)

**Measurement (broad):** primary_score <before> → <after> (<delta%>); per-category deltas: <...>
  - Same run window: git=<sha>, target=<dir>, worker=<rch host>, ts=<ISO-8601>

**Behavior-preservation:** "<test summary>; byte-identical golden/property/oracle between baseline and candidate."

**A/B verdict:** SPRT=accept samples=<n>; conformal=within band (all of p50/p95/p99/p999)

**Pattern applied:** <succinct/RLE | branchless DFA | seqlock prefix-sum | group-commit | SIMD prefilter | ...>

**Rollback:** `git revert <sha>` | flag default-off | env safety valve <VAR>
```

---

## Round-3 backfill (quantify the 8 shipped-but-unmeasured moonshots from v0.6.1)

The v0.6.1 campaign kept 8 moonshots correctness-proven but mostly UNMEASURED. Phase 0 re-benches each on a
clean host through the new bench-AB harness and records the quantified delta below (or demotes/reverts if a
clean A/B shows no real win). One clean number existed at ship: SWAR ft-p8vls −2.5% p50 ASCII.

| Moonshot | Bead | Gate | Quantified delta | Verdict |
|---|---|---|---|---|
| SWAR VTE printable scan | ft-p8vls | `bench-scalar-vte-scan` A/B | _pending Phase 0 re-bench_ | _pending_ |
| Reflow chunks + Arc SharedLines | ft-osyaf | default-active | _pending_ | _pending_ |
| Wrap-point cache | ft-3vdce | default-active | _pending_ | _pending_ |
| SoA glyph quads | ft-3r0yk | `FT_MOONSHOT_INSTANCED_GLYPH_QUADS` | _pending_ | _pending_ |
| Glyph-run interning | ft-egok5 | default-on (`FT_DISABLE_GLYPH_RUN_INTERNING`) | _pending_ | _pending_ |
| CDC dedup (codec) | ft-6c1t0 | opt-in | _pending_ | _pending_ |
| Disruptor SPSC ring | ft-87qfi | `disruptor-pane-io` | _pending_ | _pending_ |
| Succinct RLE cell attrs | ft-dkfiy | `succinct_attrs` | _pending_ (+ add byte-equiv test) | _pending_ |

---

## Entries

_None yet — round-4 campaign opening. The first kept candidate lands here._
