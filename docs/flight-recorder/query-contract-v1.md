# Flight Recorder Query Contract v1

Bead: `wa-oegrb.6.4`  
Status: Implemented (`crates/frankenterm-core/src/query_contract.rs`)

## Canonical Parameters

All search-facing surfaces (`ft search`, `ft robot search`, MCP `wa.search`) now normalize
into one canonical model:

```json
{
  "query": "string (required, FTS5 syntax)",
  "limit": "integer, default=20, range=1..1000",
  "pane": "u64 optional",
  "since": "i64 epoch-ms optional, inclusive lower bound",
  "until": "i64 epoch-ms optional, inclusive upper bound",
  "snippets": "bool, default=true",
  "mode": "\"lexical\"|\"semantic\"|\"hybrid\", default=\"lexical\""
}
```

Storage mapping is also centralized:
- `limit -> SearchOptions.limit`
- `pane -> SearchOptions.pane_id`
- `since/until -> SearchOptions.since/until`
- `snippets -> SearchOptions.include_snippets`
- snippet formatting defaults: `>>` / `<<`, max tokens `30`

## Validation Semantics

Validation is shared and deterministic:

1. `limit` must be within `1..=1000`
2. if both provided, `since <= until`
3. `query` is linted via `lint_fts_query`
4. lint `Error` findings fail parsing; lint `Warning` findings are preserved for caller display

## Error Contract

Shared validation error categories:

- `search.invalid_limit`
- `search.invalid_time_range`
- `search.invalid_query`
- `search.unsupported_mode`

Each error includes:
- a precise message
- optional actionable hint
- query lint payload (for `search.invalid_query`)

## Current Mode Support

The unified contract accepts `lexical|semantic|hybrid`, but current `ft search` / `ft robot search` /
`wa.search` execution is lexical-only. Non-lexical modes fail consistently with
`search.unsupported_mode` plus hints.

This keeps the contract forward-compatible for `wa-oegrb.6.1/.6.2/.6.3` while preserving
deterministic behavior today.

