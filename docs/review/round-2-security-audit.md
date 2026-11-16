# Round-2 security audit (CLI subset)

Re-scan of the five surfaces audited in round-1
(`docs/review/security-audit-cli.md`, commit 3b621a1f) against HEAD
24b1e315. Round-1's one finding (ft-ii8ss — wa.get_text tail
unbounded) shipped at 4994a1a9. This round looks for regressions plus
any sibling gaps the round-1 grep missed.

## Methodology (re-applied verbatim from round-1)

For each surface (a)–(e′):

1. List the commits touching it since round-1 — `git log 3b621a1f..HEAD -- <file>`.
2. Re-run the same grep recipes round-1 used.
3. Diff the findings against round-1's "hardened" / "finding" lists.
4. File a bead for any new surface that isn't already bounded.

## Per-surface verdicts

### (a) Pane-content read paths — redaction

- **Commits since round-1:** none touching `redactor.rs` or
  `read-path-redaction-matrix.md`.
- **Re-grep:** `rg 'pane.*read|get_text|render_changes' crates/frankenterm-core/src/`
  surfaces the same paths round-1 already verified are gated by the
  redactor.
- **Verdict:** ✓ **saturated**. No new read paths landed.

### (b) Policy gates — fail-closed verification

- **Commits since round-1:** policy.rs unchanged (only the
  ft-y378j.2 rename refactor at 14aa88fd touched call sites,
  not the gate logic).
- **Re-grep:** `rg 'PolicyDecision::|authorize\(' crates/frankenterm-core/src/policy.rs`
  matches the same fail-closed sites round-1 enumerated.
- **Verdict:** ✓ **saturated**.

### (c) MCP tool gates — approval bypass check

- **Commits since round-1:** 4994a1a9 (ft-ii8ss bound), 14aa88fd (rename refactor).
- **Re-grep:** the `mcp_get_text_policy_input` / `mcp_search_policy_input`
  / `mcp_send_policy_input` set still goes through `engine.authorize`
  before the action.
- **Verdict:** ✓ **saturated**.

### (d) Robot send — shell injection

- **Commits since round-1:** none.
- **Re-grep:** `shell_escape::*` is still used at every `send_text` /
  `send_keys` call site.
- **Verdict:** ✓ **saturated**.

### (e) FTS5 / SQL injection

- **Commits since round-1:** none touching `fts_query` builders.
- **Re-grep:** `params!` / parameterized queries still wrap every
  user-input → SQL boundary.
- **Verdict:** ✓ **saturated**.

### (e′) Unbounded MCP/robot input

This is where round-2 found new gaps. Round-1 listed wa.search.limit
in "Other unbounded fields not flagged" with a "trust the storage
default" rationale. With ft-ii8ss now closed at the wa.get_text
boundary, the same fail-fast-at-boundary principle should apply
elsewhere:

| handler / field           | server-side bound? | schema-advertised range           | round-2 verdict |
|---------------------------|--------------------|-----------------------------------|-----------------|
| `wa.get_text.tail`        | yes — `1..=10000`  | `minimum: 1, maximum: 10000`      | ✓ ft-ii8ss closed at 4994a1a9 |
| `wa.events.limit`         | yes — `1..=1000`   | `minimum: 1, maximum: 1000`       | ✓ pre-existing (round-1) |
| `wa.cass.limit`           | yes — `0..=1000`   | declared in code                  | ✓ pre-existing |
| **`wa.wait_for.tail`**    | **NO**             | `minimum: 0` only — schema literally says "0 = full buffer" | **⚠ NEW FINDING** |
| `wa.search.limit`         | unclear — `Option<usize>` flows into `SearchQueryInput::limit` without a handler-side cap | `Option<integer>`, no `maximum` | ⚠ UNCONFIRMED — needs trace through `SearchQueryInput::execute` |
| `wa.events.tail` (if exists) | n/a              | n/a                               | n/a — wa.events uses limit, not tail |

## Findings filed

### Finding 1: `wa.wait_for.tail` (P2)

The wa.wait_for tool's `tail` field accepts any `usize` and the
schema declares `minimum: 0` with the meaning "0 = full buffer". A
malicious or buggy MCP client can:

- Send `tail: 0` to fetch the entire scrollback for the pane on
  every wait_for call (memory pressure, especially under
  ft-fleet-memory-controller's tier-classifier hot-path).
- Send `tail: usize::MAX` for the same effect with a different
  exception path.

Same class as ft-ii8ss. The fix template is the same: server-side
`if params.tail < TAIL_MIN || params.tail > TAIL_MAX` check after
deserialization, with the `0 = full buffer` semantics replaced by an
explicit "if you want the full buffer, use the storage search
surface" hint.

Filed as `ft-ymo2i`.

### Open question: `wa.search.limit`

`SearchParams::limit: Option<usize>` reaches `SearchQueryInput::limit`
without a bound at the MCP handler level. The downstream storage
layer may have its own cap, but fail-fast-at-boundary says the
handler should bound it explicitly. Did NOT file a bead because the
trace into `SearchQueryInput::execute` is not in this scan's scope —
follow-up auditor should confirm whether the downstream layer caps
the value before deciding.

## Conclusion

Round-2 is **not saturated** — found 1 new unambiguous bound gap
(`wa.wait_for.tail`) plus 1 unconfirmed candidate (`wa.search.limit`).
Round-1's pattern (server-side fail-fast on every user-controlled
unsigned-integer field at the MCP handler boundary) is the right
template; we are still surfacing instances of the same anti-pattern
across handlers that round-1's grep didn't sweep deeply enough.

A future round-3 should:

1. Resolve the `wa.search.limit` open question.
2. Sweep every `mcp_types.rs` `pub struct *Params` for unbounded
   `usize`/`u64` fields and cross-reference against the
   `LIMIT_MIN/LIMIT_MAX`/`TAIL_MIN/TAIL_MAX` constants in
   `mcp_tools.rs`. Anything declared as a usize without a paired
   bound check is a finding.
3. Consider a CI guard analogous to ft-zoxxq.4 / ft-8smkj — refuse
   merges that introduce new `*Params` structs with unsigned-integer
   fields unless a corresponding `*_MIN`/`*_MAX` constant exists in
   the same crate.

## Cross-references

- `docs/review/security-audit-cli.md` — round-1 doc this re-scans.
- `ft-ii8ss` — round-1's only finding, closed at 4994a1a9.
- `ft-ymo2i` — round-2's finding, filed alongside this doc.
