# CLI Reference

This reference is a concise, accurate snapshot of the current command surface.
Commands marked as feature-gated require building with the corresponding feature.

## Human CLI (implemented)

### Watcher and status

```bash
wa watch [--foreground] [--auto-handle] [--poll-interval <ms>]
wa stop [--force] [--timeout <secs>]
wa status
wa list [--json]
wa show <pane_id> [--output]        # stub (not yet implemented)
wa get-text <pane_id> [--escapes]   # stub (not yet implemented)
```

### Search and events

```bash
wa search "<fts query>" [--pane <id>] [--limit <n>] [--since <epoch_ms>]
wa query "<fts query>"             # alias for wa search
wa events [--unhandled] [--pane-id <id>] [--rule-id <id>] [--event-type <type>]
wa events annotate <event_id> --note "<text>" [--by <actor>]
wa events annotate <event_id> --clear [--by <actor>]
wa events triage <event_id> --state <state> [--by <actor>]
wa events triage <event_id> --clear [--by <actor>]
wa events label <event_id> --add <label> [--by <actor>]
wa events label <event_id> --remove <label>
wa events label <event_id> --list
wa triage [--severity <error|warning|info>] [--only <section>] [--details]
```

### Actions, approvals, and audit

```bash
wa send <pane_id> "<text>" [--dry-run] [--wait-for "<pat>"] [--timeout-secs <n>]
wa send <pane_id> "<text>" --no-paste --no-newline
wa prepare send --pane-id <id> "<text>"
wa prepare workflow run <name> --pane-id <id>
wa commit <plan_id> [--text "<text>"] [--text-file <path>] [--approval-code <code>]
wa approve <code> [--pane <id>] [--fingerprint <hash>] [--dry-run]
wa audit [--limit <n>] [--pane <id>] [--action <kind>] [--decision <allow|deny|require_approval>]
```

See `docs/approvals.md` for the prepare/commit mental model and troubleshooting.

### Reservations

```bash
wa reserve <pane_id> [--ttl <secs>] [--owner-kind <workflow|agent|manual>] [--owner-id <id>]
wa reservations [--json]
```

### Workflows

```bash
wa workflow list
wa workflow run <name> --pane <id> [--dry-run]
wa workflow status <execution_id> [-v|-vv]
```

### Rules

```bash
wa rules list [--agent-type <codex|claude_code|gemini|wezterm>]
wa rules test "<text>"
wa rules show <rule_id>
```

For explain-match traces and how to interpret robot `--trace` output, see
`docs/explain-match.md`.

### Diagnostics and bundles

```bash
wa doctor
wa diag bundle [--output <dir>] [--events <n>] [--audit <n>] [--workflows <n>]
wa reproduce [--kind <crash|manual>] [--out <dir>] [--format <text|json>]
```

### Setup and config

```bash
wa setup [--list-hosts] [--dry-run] [--apply]
wa setup local
wa setup remote <host> [--yes] [--install-wa]
wa setup config
wa setup patch [--remove]
wa setup shell [--remove] [--shell <bash|zsh|fish>]

wa config init [--force]
wa config validate [--strict]
wa config show [--effective] [--json]
wa config set <key> <value> [--dry-run]
wa config export [-o <path>] [--json]
wa config import <path> [--dry-run] [--replace] [--yes]
```

### Data management

```bash
wa db migrate [--status] [--dry-run]
wa db check [-f <auto|plain|json>]
wa db repair [--dry-run] [--yes] [--no-backup]

wa backup export [-o <dir>] [--sql-dump]
wa backup import <path> [--dry-run] [--verify]

wa export <segments|events|audit|workflows|sessions> [--pane-id <id>] [--since <epoch_ms>]
```

### Learning and auth

```bash
wa learn [basics|events|workflows] [--status] [--reset]
wa auth test <service> [--account <name>] [--headful]
wa auth status <service> [--account <name>] [--all]
wa auth bootstrap <service> [--account <name>]
```

Notes:
- `wa auth` requires the `browser` feature to enable Playwright-based flows.
- `wa show` and `wa get-text` exist but are currently placeholders.

## Feature-gated commands

```bash
wa tui          # requires --features tui
wa mcp serve    # requires --features mcp
wa web          # requires --features web
wa sync         # requires --features sync
```

## Planned (not yet implemented)

```text
wa history
wa undo
```

## Robot mode (stable JSON/TOON)

Robot mode uses a stable envelope and mirrors MCP schemas.

```bash
wa robot state [--domain <name>] [--agent <type>]
wa robot get-text <pane_id> [--tail <n>] [--escapes]
wa robot send <pane_id> "<text>" [--dry-run] [--wait-for "<pat>"] [--timeout-secs <n>]
wa robot wait-for <pane_id> "<pat>" [--timeout-secs <n>] [--regex]
wa robot search "<fts query>" [--pane <id>] [--since <epoch_ms>] [--limit <n>]
wa robot events [--unhandled] [--pane <id>] [--rule-id <id>] [--event-type <type>] [--triage-state <state>] [--label <label>]
wa robot events annotate <event_id> --note "<text>" [--by <actor>]
wa robot events annotate <event_id> --clear [--by <actor>]
wa robot events triage <event_id> --state <state> [--by <actor>]
wa robot events triage <event_id> --clear [--by <actor>]
wa robot events label <event_id> --add <label> [--by <actor>]
wa robot events label <event_id> --remove <label>
wa robot events label <event_id> --list

wa robot workflow list
wa robot workflow run <name> <pane_id> [--force] [--dry-run]
wa robot workflow status [<execution_id>] [--pane <id>] [--active] [--verbose]
wa robot workflow abort <execution_id> [--reason "..."] [--force]

wa robot rules list [--pack <name>] [--agent-type <type>]
wa robot rules test "<text>" [--trace] [--pack <name>]
wa robot rules show <rule_id>
wa robot rules lint [--pack <name>] [--fixtures] [--strict]

wa robot approve <code> [--pane <id>] [--fingerprint <hash>] [--dry-run]
wa robot why <code>

wa robot reservations list
wa robot reservations reserve <pane_id> [--ttl <secs>] --owner-id <id>
wa robot reservations release <reservation_id>

wa robot accounts list [--service <openai|anthropic|google>] [--pick]
wa robot accounts refresh [--service <openai|anthropic|google>]
```

## MCP reference

MCP tools mirror robot mode. See `docs/mcp-api-spec.md` and `docs/json-schema/` for details.

Tools:
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

Resources:
- wa://panes
- wa://events
- wa://accounts
- wa://workflows
- wa://rules
- wa://reservations
