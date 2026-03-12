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

## Flow 2: why → prepare → approve → commit

Use this for mutating actions that are denied or require explicit approval.

1) Inspect the most recent policy decision:

```bash
ft why --recent require_approval --pane <pane_id>
ft why --recent denied --pane <pane_id>
```

2) Prepare a reversible plan before sending input or triggering a workflow:

```bash
ft prepare send --pane-id <pane_id> "ls"
ft prepare workflow run handle_compaction --pane-id <pane_id>
```

3) Validate or consume an approval code if policy requires one:

```bash
ft approve <approval_code> --pane <pane_id> --dry-run
ft approve <approval_code> --pane <pane_id>
```

4) Commit the prepared plan once you are satisfied with the preview:

```bash
ft commit plan:<plan_id> --text "ls"
ft commit plan:<plan_id> --approval-code <approval_code> --text "rm -rf /tmp/test"
```

Tip: `ft prepare workflow run ...` is the safest way to preview a workflow-triggered intervention before consuming approval or mutating pane state.

---

## Flow 3: triage → reproduce → file issue

Use this for crashes or persistent failures you can’t fix locally.

1) Export the latest crash bundle as an incident bundle:

```bash
ft reproduce export --kind crash
```

The incident bundle is a self-contained directory with crash report + manifest,
health snapshot (if present), and a redacted config summary when available.

2) Collect a diagnostics bundle (optional but recommended):

```bash
ft diag bundle --output /tmp/ft-diag
```

3) File an issue with:
- crash bundle path
- incident bundle path (from `ft reproduce export --kind crash`)
- triage output (plain or JSON)
- any recent ft logs

---

## Flow 4: triage → mute / noise control

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

## Flow 5: search explain → fix

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

## Flow 6: mission status → explain → resume

Use this when mission dispatch is blocked, awaiting approval, or behaving unexpectedly.

1) Inspect the current lifecycle summary:

```bash
ft mission status
ft mission status -f json
```

2) Explain degraded state, legal transitions, and assignment provenance:

```bash
ft mission explain
ft mission explain --assignment-id <assignment_id> -f json
```

3) Apply the next safe lifecycle action:

```bash
ft mission run
ft mission resume
ft mission pause --reason overload
ft mission abort --reason operator_cancel
```

Tip: mission commands default to `.ft/mission/active.json` inside the workspace. Use `--mission-file` when inspecting a saved mission artifact.

---

## Flow 7: learn → verify → resume

Use this for onboarding, refresh drills, or when you want a guided walk through the built-in operator surface.

1) Show the tutorial menu or current progress:

```bash
ft learn
ft learn --status
ft learn --achievements
```

2) Start or resume a specific track:

```bash
ft learn basics
ft learn events
ft learn workflows
ft learn robot
ft learn advanced
```

3) Record progress after completing or skipping an exercise:

```bash
ft learn --complete
ft learn --skip
```

Tip: the built-in tracks currently cover basics, events, workflows, robot mode, and advanced/search-oriented operator drills.

---

## Common commands (copy/paste)

```bash
# triage and deep-dive
ft triage
ft triage --severity error
ft why --recent --pane <pane_id>

# prepare / approve / commit
ft prepare send --pane-id <pane_id> "ls"
ft approve <approval_code> --pane <pane_id> --dry-run
ft commit plan:<plan_id> --text "ls"

# event and workflow inspection
ft events --unhandled --pane <pane_id>
ft workflow status <execution_id>

# mission control
ft mission status
ft mission explain
ft mission resume

# tutorial
ft learn --status
ft learn basics

# crash + diagnostics
ft reproduce export --kind crash
ft diag bundle --output /tmp/ft-diag
```
