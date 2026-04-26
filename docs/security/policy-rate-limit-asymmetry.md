# Policy Rate-Limit Asymmetry: ReadOutput vs SearchOutput

**Tracking bead:** `ft-jwv4z`
**Originating commit:** `88b3a9e9` (ft-zb2fl) added `ReadOutput` to the
rate-limited set without adding `SearchOutput`.
**Authoritative code:** `crates/frankenterm-core/src/policy.rs:137`
(`ActionKind::is_rate_limited`).
**Authoritative test:** `policy.rs::action_kind_rate_limited` (the
assertion `!ActionKind::SearchOutput.is_rate_limited()`).

## TL;DR

`ReadOutput` is rate-limited; `SearchOutput` is not. Both share
`AuthAction::Read`. Anyone who can call `ReadOutput` can also call
`SearchOutput` to read pane content via search snippets, even after
their `ReadOutput` token bucket is empty. This is **intentional** and
documented; the test pins the asymmetry so a future refactor cannot
flip it silently.

## What changed

`88b3a9e9` (ft-zb2fl) added `Self::ReadOutput` to the
`is_rate_limited()` match. Before that commit `ReadOutput` was treated
the same as `SearchOutput`: subject to the upstream
`authorize_read_or_search_policy` gate, but no per-token rate budget on
top of authorization. The fix added a budget to `ReadOutput` because
its bandwidth (entire pane tail per call) made bursty automated abuse
easy. `SearchOutput`'s bandwidth profile is different and the same fix
was deliberately not applied.

## Threat model

**Attacker:** an authenticated agent / actor that has already cleared
the policy authorization gate for `AuthAction::Read` on the target
pane. (An unauthenticated or unauthorized actor cannot call either
`ReadOutput` or `SearchOutput` — both are gated by
`authorize_read_or_search_policy`. This document is about post-auth
abuse, not pre-auth bypass.)

**Capability:** call `ReadOutput` until the rate budget is exhausted,
then pivot to `SearchOutput` to keep observing pane content via
matching snippets.

**Assets at risk:** pane text. The same text that `ReadOutput` would
return.

## Why we accept the pivot

1. **Authorization is the actual gate, not rate-limiting.** Both
   actions traverse `authorize_read_or_search_policy`. Rate-limiting
   `ReadOutput` blunts already-authorized abuse; it does not gate
   access. Closing the search pivot would not close any new
   authorization-level hole.
2. **Bandwidth asymmetry favors the defender.** `ReadOutput` returns
   the raw pane tail in a single call. `SearchOutput` returns only
   snippets that match a query the attacker must guess. To
   reconstruct equivalent content via `SearchOutput`, the attacker
   needs many search calls with carefully chosen queries — and each
   one is itself audited and visible to the operator.
3. **Operator workflow cost is high.** Real operators rely on bursty
   search:
   - Cockpit / dashboard refreshers re-running a saved query every
     few seconds across all panes.
   - MCP `wa.search` clients fan-querying multiple terms in parallel.
   - `ft robot search-explain` and other introspection commands.
   Binary rate-limiting `SearchOutput` would either break these or
   force a per-token budget so generous it would be cosmetic.
4. **Defense-in-depth already covers the high-value cases.**
   - Secret content in `SearchOutput` snippets is redacted via
     `policy.rs::redact_secrets` before egress.
   - Decision-log audits both `ReadOutput` and `SearchOutput` calls,
     so the pivot pattern (ReadOutput-burst → ReadOutput-deny →
     SearchOutput-burst on same pane) shows up loudly in audit.
   - Connector-side egress controls (governor, namespace isolation)
     apply uniformly to both.

## What WOULD change our minds

This decision should be revisited if any of the following becomes
true. File a follow-up against `ft-jwv4z` if you observe one:

1. `SearchOutput` snippet bandwidth grows materially (e.g., `wa.search`
   starts returning whole-segment text instead of snippets, or the
   default snippet token cap is raised). At that point the bandwidth
   argument in (2) above breaks and rate-limiting becomes worth
   considering.
2. We add a `SearchOutput` variant that bypasses redaction or returns
   non-text data (binary attachments, structured fields). Same
   reasoning.
3. We see a real incident where the
   `ReadOutput-deny → SearchOutput-pivot` pattern is used to exfiltrate
   pane content. Audit logs already surface this; a hit would be a
   forcing function.

## Implementation notes for future maintainers

If a future refactor needs to bring `SearchOutput` under a budget, do
**not** simply add `Self::SearchOutput` to `is_rate_limited`'s match.
The right shape is a new gate (`is_search_rate_limited`) with its own
configurable token budget, distinct from the per-call budget that
applies to `ReadOutput`. That preserves the operator-visible burst
profile while letting policy operators tune search separately.

The test at `policy.rs::action_kind_rate_limited` pins the current
asymmetry with `assert!(!ActionKind::SearchOutput.is_rate_limited())`
and an `assert_eq!` on shared `auth_action()`. Anyone whose change
would flip that assertion must read this document first; the test
comment links here.

## Related

- `ft-zb2fl` / `88b3a9e9` — original `ReadOutput` rate-limit add.
- `crates/frankenterm-core/src/policy.rs:240` — `ActionKind::auth_action`
  pinning that ReadOutput and SearchOutput share `AuthAction::Read`.
- `crates/frankenterm-core/src/policy.rs:5288` — the call site that
  consults `is_rate_limited()` during authorization.
