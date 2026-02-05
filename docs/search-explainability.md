# Search Explainability and Indexing Lag Troubleshooting

This guide explains how to interpret search explain output and fix common FTS
search issues. It is designed for operators who need to diagnose missing or
incomplete search results without risky actions.

## Quick triage (safe)

1. Confirm the watcher is running and panes are visible.

```bash
wa status
wa list
```

2. Run a normal search and verify the pane scope.

```bash
wa search "error"
wa search "error" --pane 3
```

3. Check FTS index health.

```bash
wa search fts verify
```

4. If results are still missing, collect diagnostics.

```bash
wa doctor
wa db check -f plain
```

5. If the index is inconsistent, rebuild it (safe but may be slow).

```bash
wa search fts rebuild
```

## Search explain output shape

Search explain results are ranked reasons with evidence and suggestions. Each
reason has a stable code, confidence score, and remediation hints.

Fields to expect:
- `query`: the original FTS query
- `pane_filter`: pane ID if a `--pane` filter was used
- `total_panes`, `observed_panes`, `ignored_panes`
- `total_segments`: total indexed segments
- `reasons[]`: ordered by confidence, each with `code`, `summary`, `evidence[]`,
  and `suggestions[]`

Plain text output is rendered like:

```text
Search explain for query: "error"
  Pane filter: 3
  Panes: 6 total (5 observed, 1 ignored)
  Indexed segments: 241

2 potential issue(s):

  1. [FTS_INDEX_INCONSISTENT] FTS index is inconsistent for 1 pane(s). Some content may not be searchable.
     pane_3_segments: 120
     pane_3_fts_rows: 95
     Suggestions:
       - Run diagnostics: wa doctor
       - The FTS index may need rebuilding.
```

## Reason codes and what to do

Use the reason codes below to self-diagnose and resolve issues:

- `NO_INDEXED_DATA`: No output has been captured yet.
  Start `wa watch` and ensure panes are active and not excluded.
- `PANE_EXCLUDED`: The requested pane is excluded by config rules.
  Adjust `ingest.panes` include/exclude rules and re-check `wa list`.
- `PANES_EXCLUDED`: One or more panes are excluded from capture.
  Review pane filters and confirm the desired panes are observed.
- `PANE_NOT_FOUND`: The pane ID is not known to the watcher.
  Verify with `wa list`; the pane may be closed or undiscovered.
- `FTS_INDEX_INCONSISTENT`: FTS rows do not match segment counts.
  Run `wa search fts verify`, then `wa search fts rebuild` if needed.
- `CAPTURE_GAPS`: Gaps were detected in captured output.
  Check `wa events --rule-id gap` and consider lowering poll interval.
- `RETENTION_CLEANUP`: Retention has removed older content.
  Review retention settings and increase retention if needed.
- `STALE_PANES`: Observed panes have not been seen recently.
  Confirm the watcher is running and panes are still open.
- `NARROW_TIME_RANGE`: Data spans less than one minute.
  Wait for more data to accumulate before re-running search.

## Indexing lag troubleshooting

Indexing lag usually shows up as low FTS row counts or inconsistent panes.
Use the sequence below to diagnose and correct it.

1. Confirm data is being captured.

```bash
wa list
wa search fts verify
```

2. If segment counts are increasing but FTS rows lag, run diagnostics.

```bash
wa doctor
```

3. If the index is inconsistent, rebuild it.

```bash
wa search fts rebuild
```

4. If lag persists, check for capture gaps or backpressure warnings.

```bash
wa events --rule-id gap
wa status
```

## Adjusting pane include/exclude rules

Pane filters live under `ingest.panes`. Excludes always win. If any include
rules exist, panes must match at least one include rule to be observed.

Example:

```toml
[ingest.panes]

[[ingest.panes.include]]
id = "ssh_panes"
domain = "SSH:*"

[[ingest.panes.exclude]]
id = "secret_cwd"
cwd = "/home/user/private"
```

After editing config:

```bash
wa config validate
wa config show --effective
wa list
```

## Safe diagnostics checklist

Use these steps before any destructive actions:

1. `wa status` to confirm watcher health.
2. `wa list` to confirm panes and ignore reasons.
3. `wa search fts verify` to detect index inconsistencies.
4. `wa doctor` for a full health snapshot.
5. `wa db check -f plain` for DB health.
6. `wa db repair --dry-run` to preview fixes (if needed).

