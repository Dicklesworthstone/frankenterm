# Redaction coverage map

**Beads:** `ft-7h5da.1.1`, `ft-7h5da.1.2`
**Audit date:** 2026-06-07

This map records where terminal-output and coordination text is redacted, which
catalog version is used, and where raw export gaps remain. It is the factual
input for the W0 redaction/corpus hygiene follow-up beads.

## Catalog terms

- **Current live catalog** means `crates/frankenterm-core/src/redactor.rs`
  `SECRET_PATTERNS`, surfaced to reports through `secret_pattern_names()`
  (`redactor.rs:581-588`). There is no separate redactor-catalog semantic
  version constant in `redactor.rs`.
- **Capture-time catalog** means the live catalog that was compiled into the
  binary at the time the row was written or fixture was harvested. Read paths
  still re-redact with the current live catalog as defense in depth.
- **Stamped `1.0`** currently appears in `.ftreplay` fixture headers
  (`replay_fixture_harvest.rs:1984-1989`), not in normal storage rows.

## Matrix

| Surface | Data carried | Redacts? | Catalog / evidence | Notes |
|---|---|---:|---|---|
| Local `output_segments` persistence | pane output | Yes | Capture-time live catalog. Writer handles `AppendSegment` through `redact_segment_for_persistence` before `append_segment_backend` (`storage.rs:8728-8747`); streaming redactor runs on chunks and boundary detection (`storage.rs:9321-9355`). | Stored segments are no longer raw for this path. |
| Distributed `output_segments` persistence | remote pane deltas | Yes | Capture-time live catalog at aggregator ingest (`main.rs:22772-22783`). | Mirrors local persistence before read-path redaction. |
| Detection events | `matched_text`, extracted JSON leaves | Yes | Current live catalog at conversion time (`runtime.rs:5014-5025`). | Comments state this applies before persistence or event-bus emission. |
| Event notes | operator notes | Yes | Current live catalog before write (`storage.rs:10006-10016`). | Labels are redacted on outbound web/MCP paths; notes are redacted at write time. |
| Audit and undo metadata | policy/audit text fields, undo payloads | Yes | Current live catalog before write for `audit_actions` and undo metadata (`storage.rs:2224-2233`, `storage.rs:2272-2287`). | This storage helper guarantee is limited to the redacted audit/undo APIs. |
| `policy_denied_audit` | MCP policy denial reason, rule, intent hash | Yes, producer-redacted | Policy-engine reason is passed through MCP denial producers (`mcp_tools.rs:896-917`, `mcp_tools.rs:1032-1052`, `mcp_tools.rs:2370-2422`); storage documents that `reason` is already policy-engine-redacted and does not re-redact (`storage.rs:2189-2194`). | Keep this as a separate boundary from storage-side redaction so future audit fixes do not overclaim where the catalog is applied. |
| Robot `state`, `get-text`, `search` | pane title/cwd, pane text, snippets/content/highlights | Yes | Current live catalog via `redact_for_output` (`main.rs:13457-13460`), pane state/text helpers (`main.rs:13467-13492`), search helper (`main.rs:13500-13509`), state include-text path (`main.rs:27552-27665`), get-text path (`main.rs:28162-28170`), and search paths (`main.rs:29007-29018`, `main.rs:29074-29096`). | Read-policy gates still run separately; redaction is not the authorization check. |
| MCP `wa.state`, `wa.get_text`, `wa.search` | pane title/cwd, pane text, query, snippets/content | Yes | Current live catalog in MCP state/output helpers (`mcp_tools.rs:2054-2085`), get-text response (`mcp_tools.rs:2449-2452`), and search response (`mcp_tools.rs:3138-3213`). | Error strings that can echo caller input are also funneled through the MCP redactor helper. |
| Web `/panes`, `/events`, `/search`, SSE | title/cwd, event fields, snippets, delta content | Yes | Current live catalog in handlers (`web/handlers.rs:66-79`, `web/handlers.rs:148-187`, `web/handlers.rs:278-286`) and SSE frames (`web/sse.rs:544-550`, `web/sse.rs:690-696`). | Search HTTP output emits snippets and content length, not full segment content. |
| `ft export` JSON/JSONL | segments and stored events | Conditional | Current live catalog only when `opts.redact` is true (`export.rs:112-130`, `export.rs:255-272`, `export.rs:433-450`). | `--redact=false` is a raw export by design. |
| `ft replay` recording export | input/output frame payloads | Conditional, default-on | Current live catalog when `opts.redact` is true (`replay.rs:900-963`). | `ExportOptions::default()` sets `redact: true`; callers can opt out. |
| Replay fixture harvest | recorder events and control/lifecycle JSON details | Yes | Harvest-time live catalog. Default harvester owns `Redactor::new()` (`replay_fixture_harvest.rs:441-453`); `harvest()` applies it to every event (`replay_fixture_harvest.rs:470-489`, `replay_fixture_harvest.rs:554-567`); text/JSON redactors are at `replay_fixture_harvest.rs:2573-2625`; `.ftreplay` header stamps `redaction_applied: true`, `redaction_version: "1.0"` (`replay_fixture_harvest.rs:1984-1989`). | Hand-authored fixtures outside this harvester are not automatically redacted by the filesystem. They must stay synthetic or have their own verifier. |
| Incident bundles | crash reports, config summaries, source JSON, pane-text summaries | Yes | Bundle-time live catalog. `write_incident_json_source` routes JSON through `write_redacted_file` (`crash.rs:2428-2464`); crash/config/source files use the same helper (`crash.rs:3860-3868`, `crash.rs:6006-6018`); pane summaries sanitize per-pane payloads before writing (`crash.rs:2647-2790`); `redaction_report.json` records counts (`crash.rs:4366-4377`). | `incident_bundle.rs` `PrivacyBudget::truncate_*` bounds size only (`incident_bundle.rs:276-325`); it is not itself a redactor. |
| Backup export `database.db` | full SQLite backup copy | Yes | Current live catalog before checksum, manifest, SQL dump, or verification (`backup.rs:504-535`). Backup redaction fingerprints `secret_pattern_names()` (`backup.rs:1069-1109`), scans text-affinity payload columns (`backup.rs:1112-1254`), updates redacted cells and clears stale `output_segments.content_hash` (`backup.rs:1283-1379`). | `manifest.json` records `redaction_catalog_version`, `redaction_patterns_checked`, and `redaction_applied`; `VACUUM` runs after changed cells so freed raw text is not retained. |
| Backup export `database.sql` | optional SQL dump | Yes | Current live catalog because the SQL dump is generated from the already-redacted backup database (`backup.rs:508-567`). Regression coverage asserts the raw synthetic secret is absent from both `database.db` and `database.sql` (`backup.rs:2368-2511`). | `ft-7h5da.1.2` closes the W0 raw backup copy/dump gap once remote proof reaches a terminal pass. |
| Static docs and arbitrary fixture files | Markdown, JSON, corpus text | Case-specific | No automatic redaction at file-write time. | Security fixtures must use synthetic secrets or a fixture-specific verifier; replay-fixture harvest is the safe automated path above. |

## Backup export closure

The W0 audit originally reproduced this gap with:

```bash
rg -n "Redactor|redact|redaction" crates/frankenterm-core/src/backup.rs
```

That is no longer expected to return no matches. The relevant closure points are:

```bash
nl -ba crates/frankenterm-core/src/backup.rs | sed -n '504,567p'
nl -ba crates/frankenterm-core/src/backup.rs | sed -n '1069,1368p'
nl -ba crates/frankenterm-core/src/backup.rs | sed -n '2357,2500p'
```

The first range shows `export_backup` calling `redact_backup_database` before
checksums and SQL dump. The second shows the backup-specific scanner and cell
updates. The third is the regression that plants a synthetic Anthropic-shaped
secret, exports with SQL dump and verify enabled, asserts `database.db` and
`database.sql` do not contain the raw secret, and round-trips the redacted backup
through import.

## Follow-up routing

- `ft-7h5da.1.3` owns derived-store/backfill behavior after a catalog change.
- `ft-7h5da.1.5` owns catalog-version stamping and attestation wiring.
