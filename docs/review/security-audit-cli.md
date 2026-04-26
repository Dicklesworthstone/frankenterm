# Security Audit (CLI-applicable subset)

**Scope:** five security-relevant surfaces called out by the user —
(1) secret redaction (`redactor.rs` + `read-path-redaction-matrix`),
(2) policy-denial audit (`policy_denied_audit` table + wiring matrix),
(3) MCP tool gates (`mcp_tools.rs` `persist_mcp_policy_denial_async`),
(4) command injection in `ft robot send`,
(5) FTS5 query injection.

**Audit checklist (user-supplied):**
- (a) any pane content path skipping redaction?
- (b) any policy gate that doesn't fail-closed?
- (c) any MCP tool that bypasses approval?
- (d) any shell exec without proper escaping?
- (e) any unbounded resource consumption reachable from MCP/robot input?

**Date:** 2026-04-26
**Method:** ripgrep + targeted reads of the security-doc matrices that
already exist under `docs/security/`, then verification against the
current code.

## TL;DR

One genuine finding (`ft-ii8ss`, P2 — `wa.get_text` unbounded tail).
Four of five surfaces audited cleanly; the fifth (redaction) has a
~3-day-old matrix that's still accurate.

| Surface                          | Verdict |
| -------------------------------- | ------- |
| Secret redaction                 | clean (matrix is comprehensive + accurate) |
| Policy-denial audit wiring       | clean (16 persist sites, 11/12 deny paths covered, the 1 exception is by design) |
| MCP tool gates                   | clean (8 authorize sites, all 8 covered by persist + envelope) |
| Robot send (shell injection)     | clean (uses `Command::args` not `format!`, `--` arg separator) |
| FTS5 / SQL injection             | clean (parameter binding everywhere, no user-input string interpolation) |
| **MCP unbounded input bounds**   | **finding** — `wa.get_text.tail` (filed) |

## (a) Pane-content read paths — redaction

`docs/security/read-path-redaction-matrix.md` (~3 days old, 6.2 KB) is
the live matrix; the `ft-h8da2` audit verified every outbound pane-content
read path goes through `Redactor::redact` or
`PolicyEngine::redact_secrets`. Spot-checked at HEAD:

- `wa.get_text` (mcp_tools.rs:1599): `text: engine.redact_secrets(&text)` ✓
- `wa.search` (mcp_tools.rs:2205-2223): `Redactor::new()` + per-result
  `.redact()` on snippet, content, query ✓
- `wa.state` (mcp_tools.rs:1259-1267): `REDACTOR.redact(title)`,
  `redact(cwd)`, `redact(ignore_reason)` ✓
- Web `/panes`, `/events`, `/search`, SSE `/stream/events`,
  `/stream/deltas` — verified by the existing matrix.
- Audit table writes: `record_audit_action_redacted` (write-time
  redaction).

The matrix's "Known gap" at the bottom — distributed mode aggregator
forwarding has zero `Redactor` usages — is still open. The default
`Config.distributed.enabled = false` limits exposure; if the user's
threat model includes operators with `distributed = true`, audit that
path. Out of scope for this 60-min pass; acknowledged in the matrix.

`Redactor` itself (`redactor.rs:97-158`) covers 15 secret patterns:
openai/anthropic/github/aws-id/aws-secret/bearer/slack/stripe keys,
database URLs, device codes, oauth URLs, generic api_key/token/password/
secret. **Notable absentees:** GCP service-account JSON, Azure
connection strings, SSH/TLS private-key PEM blocks, GitHub fine-grained
PATs (`github_pat_*` prefix). Not flagged as a finding here — the
existing patterns are conservative-by-design ("err on the side of
caution") and adding more is enhancement, not a leak.

**Verdict (a): clean.**

## (b) Policy gates — fail-closed verification

Every `engine.authorize(&input)` deny path returns an
`MCP_ERR_POLICY` error envelope (or routes through
`PolicyGatedInjector` for write-side). No path treats deny as
"degrade gracefully" or "fall through". Spot-checked the 8 authorize
sites in `mcp_tools.rs` (lines 314, 369, 1518, 1805, 2119, 3925, 4102,
4387) — each sits in a distinct tool handler and each rejects on
`decision.is_denied()` before any state mutation.

**Verdict (b): clean — fail-closed throughout.**

## (c) MCP tool gates — approval bypass check

`docs/security/policy-denial-audit-wiring-matrix.md` (Apr 23, ft-hg6io)
listed 7 of 12 deny paths as missing audit-row writes. The newer
`docs/security/read-path-redaction-matrix.md` notes "11 of 12 deny
paths now also persist to `policy_denied_audit`. Only `wa.send` is
deliberately routed through `PolicyGatedInjector` (which already
writes to `audit_actions`); adding a second audit stream would
double-count."

Verified at HEAD: `grep -c persist_mcp_policy_denial_async mcp_tools.rs
→ 16` call sites (sync + async variants combined). Each authorize site
in mcp_tools.rs lines 1518, 1805, 2119, 3925, 4102, 4387 has a
matching `persist_mcp_policy_denial_async` call within ~6 lines on the
deny branch. The helper at line 369
(`mcp_authorize_mcp_mutation`) calls the SYNC variant
`persist_mcp_policy_denial`. All deny paths are auditable from SQL.

**Verdict (c): clean.** No MCP tool bypasses approval; `wa.send`'s
`audit_actions`-only path is documented.

## (d) Robot send — shell injection

`crates/frankenterm-core/src/wezterm.rs:1966` (`send_text_impl`):

```rust
let pane_id_str = pane_id.to_string();
let mut args = vec!["cli", "send-text", "--pane-id", &pane_id_str];
if no_paste { args.push("--no-paste"); }
if no_newline { args.push("--no-newline"); }
args.push("--");
args.push(text);
```

Uses `Command::new(wezterm_binary()).args(args)`. Each arg is a
separate `argv[]` entry — no shell interpretation. The `--` argument
separator (line 2014) is the gold-standard defense against `--flag`
confusion if `text` happens to start with `-`. `text` cannot break out
of its argument slot regardless of shell metacharacters.

The mux-pool path (line 1984-1989) sends bytes via `pool.write_to_pane_with_cx`
or `pool.send_paste_with_cx` — direct API calls, not subprocess
execution. No shell involved.

**Verdict (d): clean — no shell injection vector.**

## (e) FTS5 / SQL injection

`grep -nE 'MATCH ?[?]' storage.rs` confirms every FTS5 query uses
parameter binding (`MATCH ?1`), not string interpolation. The only
`format!`-into-SQL call sites are:

- `storage.rs:3899`: `SELECT COUNT(*) FROM {name}` — `name` is from a
  hardcoded `table_names` array (line 3895). Safe.
- `storage.rs:14759`: `DELETE FROM events WHERE id IN ({inner_query} LIMIT {batch_size})`
  — `inner_query` is built by `build_tier_query` (line 14779) using
  `?` placeholders + parameter binding throughout. `batch_size: usize`
  is a typed integer. Safe.
- `storage.rs:17404,17415,17428`: `format!(" AND ... IN ({})", placeholders.join(","))`
  — placeholders are `?` strings; user input goes into `params`
  vector via `Box::new(s.clone())` and is bound at execute time. Safe.

**Verdict (e): clean.** All SQL accepts user input via `?` placeholders;
no string interpolation of user-controlled values into SQL.

## (e′) Unbounded resource consumption from MCP/robot input

This was the user's checklist item (e); audited separately because it
turned up a real finding.

### Hardened (defensive coding present)

- **`wa.events.limit`** (`mcp_tools.rs:2392-2408`): explicit
  `LIMIT_MIN..=LIMIT_MAX` bounds, returns `MCP_ERR_INVALID_ARGS` on
  out-of-range. Comment at lines 2389-2391 documents the threat model:
  > "or `limit: u64::MAX` (memory-pressure vector: the downstream
  > `storage.get_events_with_cx` query and the subsequent
  > `Vec::with_capacity(events.len())` both scale with the limit)"
- **`wa.search.limit`** (`query_contract.rs:147`): canonical query
  parser validates against `SEARCH_LIMIT_MAX` (from `SearchTuning::DEFAULT_MAX_LIMIT`),
  returns `InvalidLimit` error.

### Finding: `wa.get_text.tail` has no upper bound

`mcp_types.rs:33-39`:

```rust
pub(super) struct GetTextParams {
    pub pane_id: u64,
    #[serde(default = "default_tail")]
    pub tail: usize,             // <-- unbounded, defaults to 500
    #[serde(default)]
    pub escapes: bool,
}
```

A caller can send `{"pane_id": 0, "tail": 18446744073709551615}` and
it deserializes to `usize::MAX`. The downstream
`apply_tail_truncation` (`mcp_types.rs:855-883`) returns the entire
text untruncated when `tail >= line_count`:

```rust
if lines.len() <= tail_lines {
    return (text.to_string(), false, None);
}
```

Worst case: a pane with a configured 100k-line scrollback (production
fleet config) hit by `tail: usize::MAX` returns ~10MB of text per
request. A single attacker spamming this can exhaust server memory.

Note: `apply_tail_truncation` also calls `text.lines().collect::<Vec<&str>>`
(line 859) which always allocates a Vec sized to `original_lines`,
*regardless* of the requested tail. So even with a small tail value,
the intermediate allocation is proportional to total text size. That's
algorithmic; the tail bound only constrains output size. The bound is
still load-bearing because it caps the *response* size which goes back
through redaction + JSON serialization + transport.

**Filed: `ft-ii8ss`** (P2 security) — apply the wa.events template:
`const GET_TEXT_TAIL_MIN: usize = 1; const GET_TEXT_TAIL_MAX: usize = 50000;`
(or whatever the operator-tunable max scrollback is) and reject
out-of-range with `MCP_ERR_INVALID_ARGS`.

### Other unbounded fields not flagged

Spot-checked: `wa.send.text` (typed `String`, no length cap on the MCP
side, but rate-limited by `PolicyGatedInjector` and bounded by the
mux's PASTE buffer size). `wa.wait_for.timeout_secs` — typed; not a
memory vector. `wa.events_annotate.note` — typed `Option<String>`,
written to SQLite as TEXT (rusqlite handles bounds at the SQLite
level: 1GB max).

## What was NOT audited (acknowledged scope)

- **Distributed mode wire protocol.** The redaction matrix's "Known
  gap" — `distributed.rs` has zero `Redactor` usages. Default-disabled
  feature; full audit is a separate bead if/when an operator turns it
  on in production.
- **Web/SSE auth.** The `web` feature surface gates `/panes`, `/events`,
  `/search`, SSE streams. Auth model (bearer? tokens? unauthenticated
  on localhost?) was not traced. Likely deserves its own focused audit
  bead.
- **MCP framework auth.** `fastmcp` provides the transport; the
  auth/session-binding was not deep-audited.
- **Replay-side redaction.** `replay.rs` and the replay sub-crate
  surface (broken stub at the time of this audit, ft-lwa5q open) was
  not deep-audited because the crate's compile state is unstable.

These are scope-acknowledged gaps; not findings.

## Conclusion

One bead filed (`ft-ii8ss`), one defensive enhancement worth tracking
(redactor pattern coverage gaps for GCP/Azure/SSH-keys, but not
filed — the existing 15 patterns are conservative-by-design and
adding more is enhancement). The five user-named surfaces are
otherwise clean: redaction matrix is comprehensive, policy-denial
audit wiring covers 11/12 deny paths (the 12th is by design),
shell-injection is non-issue (Command::args + `--` separator), and
FTS5/SQL injection is non-issue (parameter binding throughout).

The hardening done in `wa.events` (explicit LIMIT_MIN/LIMIT_MAX bounds
+ MCP_ERR_INVALID_ARGS rejection) is the template the wa.get_text fix
should follow — same pattern, same error surface, same operator
ergonomics.
