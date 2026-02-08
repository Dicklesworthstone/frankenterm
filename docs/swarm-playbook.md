# Swarm Playbook (Robot + MCP)

Use this playbook when AI agents are operating `wa` directly.

## Choose Interface

- Use `wa robot` when the agent can run shell commands directly.
- Use MCP when the agent is already in a tool-calling runtime and should avoid shell parsing.
- Prefer `--format toon` for AI-to-AI loops when JSON structure is not required.
- Prefer `--format json` when responses are fed into strict schema checks.

## Canonical Loop (Robot CLI)

1. Discover panes and pick scope.
```bash
wa robot --format toon state
```
2. Read recent pane output for current context.
```bash
wa robot --format toon get-text <pane_id> --tail 80
```
3. Search historical capture before acting.
```bash
wa robot --format toon search "error OR failed" --pane <pane_id> --limit 10
```
4. Triage and mutate events when needed.
```bash
wa robot --format json events --unhandled --limit 20
wa robot --format json events annotate <event_id> --note "Investigating root cause"
wa robot --format json events triage <event_id> --state investigating
wa robot --format json events label <event_id> --add urgent
```
5. Run workflow automation only after context is clear.
```bash
wa robot --format json workflow list
wa robot --format json workflow run <name> <pane_id> --dry-run
wa robot --format json workflow run <name> <pane_id>
wa robot --format json workflow status --pane <pane_id> --active
```
6. When injecting input, verify post-conditions.
```bash
wa robot --format json send <pane_id> "command" --dry-run
wa robot --format json send <pane_id> "command" --wait-for "pattern" --timeout-secs 30
```

## Canonical Loop (MCP Tool Calls)

1. `wa.state` to select pane targets.
2. `wa.get_text` for local context.
3. `wa.search` for prior evidence.
4. `wa.events` then `wa.events_annotate` / `wa.events_triage` / `wa.events_label`.
5. `wa.workflow_list` and `wa.workflow_run`.
6. `wa.send` with `dry_run=true` before real sends.
7. `wa.wait_for` or `wa.workflow_status` for verification.

Tool contracts and parameter schemas are defined in `docs/mcp-api-spec.md` and `docs/json-schema/`.

## Safety Rules

- Never assume a pane is safe for typing; policy may deny alt-screen or prompt-inactive panes.
- Always keep one verification step after any mutation or send.
- Treat `ok=false` as a first-class branch; read `error_code` and `hint`.
- Keep notes redaction-safe; secrets should never appear in notes or prompts.
- Use workflow dry-run for high-risk actions and require human approval when policy asks.

## Minimal Prompt Snippet

Use this in an agent system prompt when delegating to `wa`:

```text
Use wa robot/MCP as the control plane.
Loop: state -> get-text -> search -> events -> mutate annotations -> workflow/send -> verify.
Default to toon for inspection and json for strict parsing.
Respect policy denials and approval requirements; do not bypass safety gates.
```
