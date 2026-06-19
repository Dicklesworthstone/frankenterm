# Round-5 Negative-Evidence Ledger

> The Alien Optimization Gauntlet (v0.8.0 campaign). **Load-bearing:** every round-5 optimization that
> is *rejected*, *reverted*, or *measured-as-no-win* gets an entry here closed with exactly one of the 8
> grep-able **retry-condition predicate** forms — so the next agent who greps the touched symbol finds
> precisely what evidence would unblock a retry. Negative evidence is a *win*, not a failure.

The **10 keep-gate rules**, the **8 retry-condition forms**, the **forbidden anti-vocabulary**, and the
rejected-entry template are defined once in
[`round4-negative-results.md`](round4-negative-results.md) — they carry over unchanged. Kept/promoted
changes → [`round5-keep-ledger.md`](round5-keep-ledger.md). Campaign record →
[`../../tests/artifacts/perf/v080-round5-campaign.md`](../../tests/artifacts/perf/v080-round5-campaign.md).

Round-5 nuance for the 19 flags: a round-4 flag that, when finally A/B-measured on the quiet Mac, shows
**no keep-gate win** is NOT reverted (it is already default-OFF and zero-risk) — it gets an entry here
recording the measured delta + cv + the retry-condition form that would justify a future default-on
promotion. Only a measured **regression on a default-on path** triggers an actual `git revert`.

---

## Entries

### 2026-06-19 | Q1 prefix-sum — default-ON promotion BLOCKED on cv (the win itself is real, 32.5×)

**Status:** cv-blocked-for-promotion (NOT a reject — the optimization is a measured 32.5× win, kept
default-off; see round5-keep-ledger.md). The blocker is purely the keep-gate rule-8 cv threshold for an
auto-promotion to default-on.
**Measurement:** −96.92% (3.09ms→95µs), p=0, but candidate cv=15.2% / baseline cv=20.6% > 5% (Mac not
quiet — concurrent swarm + tend). Distributions are non-overlapping so the win is unambiguous.
**Retry-condition predicate (Form 5):** Do not promote Q1 to default-on from this noisy reading; re-run
the `scrollback_prefix_index` env A/B on a genuinely quiet Mac (swarm idle / converged) plus a
shallow-scrollback non-regression bench, and promote only once candidate cv≤5 AND the shallow case is
non-regressed. Until then Q1 ships default-off (zero-risk) with this 32.5× deep-scroll evidence on record.
**Rollback:** n/a (default-off, never promoted).

---

_(round-5 measured-no-win / reject / revert entries land below as A/B runs complete on the quiet host.
M6 persistent COW grid stays governed by its round-4 entry until the E1 concurrent-search bench produces
contention evidence; E2 will either escalate M6 or refresh that entry here with the measured numbers.)_
