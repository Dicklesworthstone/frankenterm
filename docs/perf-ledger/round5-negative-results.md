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

_(round-5 measured-no-win / reject / revert entries land below as A/B runs complete on the quiet host.
M6 persistent COW grid stays governed by its round-4 entry until the E1 concurrent-search bench produces
contention evidence; E2 will either escalate M6 or refresh that entry here with the measured numbers.)_
