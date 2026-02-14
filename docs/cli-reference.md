# CLI Reference

This reference is a concise, accurate snapshot of the current command surface.
Commands marked as feature-gated require building with the corresponding feature.

## Human CLI (implemented)

### Watcher and status

```bash
ft watch [--foreground] [--auto-handle] [--poll-interval <ms>]
ft stop [--force] [--timeout <secs>]
ft status
ft list [--json]
ft show <pane_id> [--output]        # stub (not yet implemented)
ft get-text <pane_id> [--tail <n>] [--escapes]
```

### Search and events

```bash
ft search "<fts query>" [--pane <id>] [--limit <n>] [--since <epoch_ms>] [--until <epoch_ms>] [--mode <lexical|semantic|hybrid>]
ft query "<fts query>"             # alias for ft search
ft events [--unhandled] [--pane-id <id>] [--rule-id <id>] [--event-type <type>]
ft events annotate <event_id> --note "<text>" [--by <actor>]
ft events annotate <event_id> --clear [--by <actor>]
ft events triage <event_id> --state <state> [--by <actor>]
ft events triage <event_id> --clear [--by <actor>]
ft events label <event_id> --add <label> [--by <actor>]
ft events label <event_id> --remove <label>
ft events label <event_id> --list
ft triage [--severity <error|warning|info>] [--only <section>] [--details]
```

Mode notes:
- `lexical` uses FTS5/BM25 ranking.
- `semantic` uses embedding-backed retrieval with fused ranking score output.
- `hybrid` fuses lexical + semantic lanes with deterministic rank fusion.

### Actions, approvals, and audit

```bash
ft send <pane_id> "<text>" [--dry-run] [--wait-for "<pat>"] [--timeout-secs <n>]
ft send <pane_id> "<text>" --no-paste --no-newline
ft prepare send --pane-id <id> "<text>"
ft prepare workflow run <name> --pane-id <id>
ft commit <plan_id> [--text "<text>"] [--text-file <path>] [--approval-code <code>]
ft approve <code> [--pane <id>] [--fingerprint <hash>] [--dry-run]
ft audit [--limit <n>] [--pane <id>] [--action <kind>] [--decision <allow|deny|require_approval>]
```

See `docs/approvals.md` for the prepare/commit mental model and troubleshooting.

### Reservations

```bash
ft reserve <pane_id> [--ttl <secs>] [--owner-kind <workflow|agent|manual>] [--owner-id <id>]
ft reservations [--json]
```

### Workflows

```bash
ft workflow list
ft workflow run <name> --pane <id> [--dry-run]
ft workflow status <execution_id> [-v|-vv]
```

### Rules

```bash
ft rules list [--agent-type <codex|claude_code|gemini|wezterm>]
ft rules test "<text>"
ft rules show <rule_id>
```

For explain-match traces and how to interpret robot `--trace` output, see
`docs/explain-match.md`.

### Diagnostics and bundles

```bash
ft doctor
ft diag bundle [--output <dir>] [--events <n>] [--audit <n>] [--workflows <n>]
ft reproduce [--kind <crash|manual>] [--out <dir>] [--format <text|json>]
```

### Setup and config

```bash
ft setup [--list-hosts] [--dry-run] [--apply]
ft setup local
ft setup remote <host> [--yes] [--install-wa]
ft setup config
ft setup patch [--remove]
ft setup shell [--remove] [--shell <bash|zsh|fish>]

ft config init [--force]
ft config validate [--strict]
ft config show [--effective] [--json]
ft config set <key> <value> [--dry-run]
ft config export [-o <path>] [--json]
ft config import <path> [--dry-run] [--replace] [--yes]
```

### Data management

```bash
ft db migrate [--status] [--dry-run]
ft db check [-f <auto|plain|json>]
ft db repair [--dry-run] [--yes] [--no-backup]

ft backup export [-o <dir>] [--sql-dump]
ft backup import <path> [--dry-run] [--verify]

ft export <segments|events|audit|workflows|sessions> [--pane-id <id>] [--since <epoch_ms>]
```

### Learning and auth

```bash
ft learn [basics|events|workflows] [--status] [--reset]
ft auth test <service> [--account <name>] [--headful]
ft auth status <service> [--account <name>] [--all]
ft auth bootstrap <service> [--account <name>]
```

Notes:
- `ft auth` requires the `browser` feature to enable Playwright-based flows.
- `ft show` exists but is still a placeholder.

## Feature-gated commands

```bash
ft tui          # requires --features tui
ft mcp serve    # requires --features mcp
ft web          # requires --features web
ft sync         # requires --features sync
```

## Planned (not yet implemented)

```text
ft history
ft undo
```

## Robot mode (stable JSON/TOON)

Robot mode uses a stable envelope and mirrors MCP schemas.

```bash
ft robot state [--domain <name>] [--agent <type>]
ft robot get-text <pane_id> [--tail <n>] [--escapes]
ft robot send <pane_id> "<text>" [--dry-run] [--wait-for "<pat>"] [--timeout-secs <n>]
ft robot wait-for <pane_id> "<pat>" [--timeout-secs <n>] [--regex]
ft robot search "<fts query>" [--pane <id>] [--since <epoch_ms>] [--until <epoch_ms>] [--limit <n>] [--snippets[=<true|false>]] [--mode <lexical|semantic|hybrid>]
ft robot events [--unhandled] [--pane <id>] [--rule-id <id>] [--event-type <type>] [--triage-state <state>] [--label <label>]
ft robot events annotate <event_id> --note "<text>" [--by <actor>]
ft robot events annotate <event_id> --clear [--by <actor>]
ft robot events triage <event_id> --state <state> [--by <actor>]
ft robot events triage <event_id> --clear [--by <actor>]
ft robot events label <event_id> --add <label> [--by <actor>]
ft robot events label <event_id> --remove <label>
ft robot events label <event_id> --list

ft robot workflow list
ft robot workflow run <name> <pane_id> [--force] [--dry-run]
ft robot workflow status [<execution_id>] [--pane <id>] [--active] [--verbose]
ft robot workflow abort <execution_id> [--reason "..."] [--force]

ft robot rules list [--pack <name>] [--agent-type <type>]
ft robot rules test "<text>" [--trace] [--pack <name>]
ft robot rules show <rule_id>
ft robot rules lint [--pack <name>] [--fixtures] [--strict]

ft robot approve <code> [--pane <id>] [--fingerprint <hash>] [--dry-run]
ft robot why <code>

ft robot reservations list
ft robot reservations reserve <pane_id> [--ttl <secs>] --owner-id <id>
ft robot reservations release <reservation_id>

ft robot accounts list [--service <openai|anthropic|google>] [--pick]
ft robot accounts refresh [--service <openai|anthropic|google>]
```

Examples:
- `ft robot search "compilation failed" --mode lexical`
- `ft robot search "compilation failed" --mode semantic`
- `ft robot search "compilation failed" --mode hybrid`

Policy/redaction:
- `ft get-text`, `ft search`, `ft robot get-text`, and `ft robot search` are policy-gated read/query surfaces.
- Returned text/snippets are passed through the standard secret redactor before output.
- Redaction applies to echoed query/content fields as well (`query`, `snippet`, `content`) for search responses.
- Policy denials return `robot.policy_denied`; approval-required paths return `robot.require_approval` with approval guidance.

## MCP reference

MCP tools mirror robot mode. See `docs/mcp-api-spec.md` and `docs/json-schema/` for details.

Tools (tool IDs currently still use the `wa.*` prefix):
- wa.state
- wa.get_text
- wa.send
- wa.wait_for
- wa.search
- wa.events
- wa.events_annotate
- wa.events_triage
- wa.events_label
- wa.workflow_run
- wa.accounts
- wa.accounts_refresh
- wa.rules_list
- wa.rules_test
- wa.rules_show
- wa.rules_lint
- wa.reserve
- wa.release
- wa.reservations
- wa.approve
- wa.why
- wa.workflow_list
- wa.workflow_status
- wa.workflow_abort

Resources (resource URIs currently still use the `wa://` scheme):
- wa://panes
- wa://events
- wa://accounts
- wa://workflows
- wa://rules
- wa://reservations
