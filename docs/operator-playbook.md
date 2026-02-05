# Operator Playbook (triage → why → reproduce)

This playbook is a pragmatic guide for keeping wa healthy during day-to-day use.
It focuses on fast diagnosis, safe remediation, and actionable artifacts.

## Quick start

```bash
wa triage
wa triage -f json
```

If something needs attention, follow the relevant flow below.

---

**Crash-Only Behavior + Crash Bundles**
wa treats a crash as an observable event with artifacts, not a silent failure.
On panic, the watcher writes a bounded, redacted crash bundle and then exits.

Crash bundle facts:
- Default location: `<workspace>/.wa/crash/wa_crash_YYYYMMDD_HHMMSS/`
- Files included: `manifest.json`, `crash_report.json`, and `health_snapshot.json` (if available)
- Redaction: all text is passed through the policy redactor before writing
- Size bounds: backtrace truncated to 64 KiB, total bundle capped at 1 MiB

Where to find the crash directory:
- It lives under the workspace root. Use `wa config show` or `wa status` to confirm the workspace path.
- You can change the workspace via `--workspace` or `WA_WORKSPACE` if you need bundles elsewhere.

---

## Flow 1: triage → why → fix

Use this for unhandled events or workflows that need intervention.

1) Triage to find the affected pane/event:

```bash
wa triage --severity warning
wa events --unhandled --pane <pane_id>
```

2) Explain the detection:

```bash
wa why --recent --pane <pane_id>
# optional deep dive on a specific decision
wa why --recent --pane <pane_id> --decision-id <id>
```

3) Fix with an explicit action (examples):

```bash
# handle compaction event
wa workflow run handle_compaction --pane <pane_id>

# check a workflow that looks stuck
wa workflow status <execution_id>
```

Tip: If you are unsure, run workflows with `--dry-run` first.

---

## Flow 2: triage → reproduce → file issue

Use this for crashes or persistent failures you can’t fix locally.

1) Export the latest crash bundle as an incident bundle:

```bash
wa reproduce --kind crash
```

The incident bundle is a self-contained directory with crash report + manifest,
health snapshot (if present), and a redacted config summary when available.

2) Collect a diagnostics bundle (optional but recommended):

```bash
wa diag bundle --output /tmp/wa-diag
```

3) File an issue with:
- crash bundle path
- incident bundle path (from `wa reproduce --kind crash`)
- triage output (plain or JSON)
- any recent wa logs

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
# ~/.config/wa/wa.toml
[patterns.pack_overrides.core]
disabled_rules = ["core.codex:usage_reached"]
```

Apply changes and reload if needed:

```bash
wa config validate
wa config reload
```

Note: Disabling rules prevents those detections from firing entirely.

---

## Flow 4: search explain → fix

Use this for missing or incomplete search results.

1) Run safe checks:

```bash
wa search "error"
wa search fts verify
wa doctor
```

2) If the index is inconsistent, rebuild:

```bash
wa search fts rebuild
```

3) For detailed reason codes and remediation, see `docs/search-explainability.md`.

---

## Common commands (copy/paste)

```bash
# triage and deep-dive
wa triage
wa triage --severity error
wa why --recent --pane <pane_id>

# event and workflow inspection
wa events --unhandled --pane <pane_id>
wa workflow status <execution_id>

# crash + diagnostics
wa reproduce --kind crash
wa diag bundle --output /tmp/wa-diag
```
