# Operator Playbook (triage → why → reproduce)

This playbook is a pragmatic guide for keeping ft healthy during day-to-day use.
It focuses on fast diagnosis, safe remediation, and actionable artifacts.

## Quick start

```bash
ft triage
ft triage -f json
```

If something needs attention, follow the relevant flow below.

---

**Crash-Only Behavior + Crash Bundles**
ft treats a crash as an observable event with artifacts, not a silent failure.
On panic, the watcher writes a bounded, redacted crash bundle and then exits.

Crash bundle facts:
- Default location: `<workspace>/.ft/crash/ft_crash_YYYYMMDD_HHMMSS/`
- Files included: `manifest.json`, `crash_report.json`, and `health_snapshot.json` (if available)
- Redaction: all text is passed through the policy redactor before writing
- Size bounds: backtrace truncated to 64 KiB, total bundle capped at 1 MiB

Where to find the crash directory:
- It lives under the workspace root. Use `ft config show` or `ft status` to confirm the workspace path.
- You can change the workspace via `--workspace` or `FT_WORKSPACE` if you need bundles elsewhere.

---

## Flow 1: triage → why → fix

Use this for unhandled events or workflows that need intervention.

1) Triage to find the affected pane/event:

```bash
ft triage --severity warning
ft events --unhandled --pane <pane_id>
```

2) Explain the detection:

```bash
ft why --recent --pane <pane_id>
# optional deep dive on a specific decision
ft why --recent --pane <pane_id> --decision-id <id>
```

3) Fix with an explicit action (examples):

```bash
# handle compaction event
ft workflow run handle_compaction --pane <pane_id>

# check a workflow that looks stuck
ft workflow status <execution_id>
```

Tip: If you are unsure, run workflows with `--dry-run` first.

---

## Flow 2: triage → reproduce → file issue

Use this for crashes or persistent failures you can’t fix locally.

1) Export the latest crash bundle as an incident bundle:

```bash
ft reproduce --kind crash
```

The incident bundle is a self-contained directory with crash report + manifest,
health snapshot (if present), and a redacted config summary when available.

2) Collect a diagnostics bundle (optional but recommended):

```bash
ft diag bundle --output /tmp/ft-diag
```

3) File an issue with:
- crash bundle path
- incident bundle path (from `ft reproduce --kind crash`)
- triage output (plain or JSON)
- any recent ft logs

---

## Flow 3: triage → mute / noise control

If an event is noisy but safe, reduce noise without losing observability.

### TUI mute (fastest)

In the TUI triage view:
- Select the event
- Press `m` to mark it handled (muted)

### Disable specific rules (config)

You can silence a specific detection rule via pack overrides:

```toml
# ~/.config/ft/ft.toml
[patterns.pack_overrides.core]
disabled_rules = ["core.codex:usage_reached"]
```

Apply changes and reload if needed:

```bash
ft config validate
ft config reload
```

Note: Disabling rules prevents those detections from firing entirely.

---

## Flow 4: search explain → fix

Use this for missing or incomplete search results.

1) Run safe checks:

```bash
ft search "error"
ft search fts verify
ft doctor
```

2) If the index is inconsistent, rebuild:

```bash
ft search fts rebuild
```

3) For detailed reason codes and remediation, see `docs/search-explainability.md`.

---

## Common commands (copy/paste)

```bash
# triage and deep-dive
ft triage
ft triage --severity error
ft why --recent --pane <pane_id>

# event and workflow inspection
ft events --unhandled --pane <pane_id>
ft workflow status <execution_id>

# crash + diagnostics
ft reproduce --kind crash
ft diag bundle --output /tmp/ft-diag
```
