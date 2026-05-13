# Operator Playbook (triage → why → reproduce)

This playbook is a pragmatic guide for keeping ft healthy during day-to-day use.
It focuses on fast diagnosis, safe remediation, and actionable artifacts.

Operator reality check:
- `ft doctor`, `ft status --health`, `ft triage`, crash bundles, search, and the local evidence surfaces are native `ft` operator paths.
- Live pane/session operations still rely on the current WezTerm-backed mux interop boundary.
- Support and verification claims in this guide are anchored by `docs/ft-xbnl0-verification-contract.md`, `docs/ft-xbnl0-4-6-completion-evidence.md`, and `docs/ft-xbnl0-5-7-completion-evidence.md`.

## Quick start

```bash
ft triage
ft triage -f json
ft status --health
ft robot events --limit 20
```

If something needs attention, follow the relevant flow below.

Related guides:
- For blessed 10, 50, and 200+ pane profiles plus exact validation commands, use `docs/ft-xbnl0-5-3-blessed-tuning-playbook.md`.
- For release posture and per-tier fallback defaults, use `docs/resize-user-facing-release-tuning-guidance-wa-1u90p.8.5.md`.
- For exact knob meanings and safe starting ranges, use `docs/tuning-reference.md`.
- For GUI-specific fleet operation, use `docs/frankenterm-gui-user-guide.md`.
- For the supported-path honesty sweep, use `docs/ft-xbnl0-3-6-supported-path-truth-sweep.md`.

---

## First Run / Bootstrap

Use this path on a fresh host or after any environment drift.

```bash
ft doctor
ft doctor --json
ft status --health
wezterm cli list
ft watch --foreground
ft robot state
```

Interpretation:
- `ft doctor` / `ft status --health` validate the local runtime, workspace, database, and operator guidance surfaces immediately.
- `wezterm cli list` validates the current live mux interop boundary for pane/session operations.
- `ft watch --foreground` is the fastest way to force first-run bootstrap of `.ft/`, logs, and the database while keeping diagnostics visible.
- `ft robot state` confirms that the watcher plus live pane discovery path agree on what is observable.

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

## Flow 3b: high memory / residency incident

Use this when FrankenTerm or a mux process has high RSS, memory pressure, or
unexpected growth. Do not call it a heap leak until the residency buckets below
have evidence.

1) Capture the ft-side state first:

```bash
ft triage -f json > /tmp/ft-memory-triage.json
ft status --health > /tmp/ft-memory-status-health.txt
ft doctor --json > /tmp/ft-memory-doctor.json
ft robot events --limit 100 > /tmp/ft-memory-events.json
ft diag bundle --output /tmp/ft-memory-diag
```

If the build exposes the resource-pressure cockpit, also capture:

```bash
ft robot capacity --level 2 > /tmp/ft-memory-cockpit.json
```

2) Identify the process tree and resident memory:

```bash
ps -axo pid,ppid,rss,vsz,comm | rg 'frankenterm|ft |wezterm'
```

For the suspected process, collect platform diagnostics:

```bash
# macOS native process evidence
vmmap <pid> -summary > /tmp/frankenterm-<pid>.vmmap.txt
/usr/bin/sample <pid> 5 -file /tmp/frankenterm-<pid>.sample.txt
heap <pid> > /tmp/frankenterm-<pid>.heap.txt
```

3) Classify the bytes before choosing a fix:

| Bucket | Evidence to look for | Next move |
| --- | --- | --- |
| `rust_heap` | `heap` growth, allocator buckets, long-lived Rust structures | File a heap-retention issue with sample + heap output. |
| `mmap_file_backed` | `vmmap` file mappings, Tantivy/cold-tier mappings | Check cache pressure and whether RSS is reclaimable file-backed memory. |
| `sqlite_page_cache` | SQLite mappings, page-cache residency, WAL/page-cache evidence | Check storage pressure before blaming heap or scrollback retention. |
| `graphics_media` | GPU, image, font, or render/media segments | Use GUI/render runbooks before changing scrollback or heap code. |
| `scrollback_cache` | cockpit memory tiers, gap bursts, warm/cold tier pressure | Tune retention, capture cadence, or tier budgets with proof artifacts. |
| `child_processes` | child RSS in `ps` tree | Attribute to the child before changing ft memory policy. |
| `unknown` | non-zero unknown cockpit row or unclassified `vmmap` regions | Add drilldown evidence; do not hide it under a generic leak label. |

The resource-pressure cockpit contract is
`docs/resource-pressure-cockpit-contract.md`. The first reduced RCH proof lane is
`RCH_STEP_TIMEOUT_SECS=1800 tests/e2e/test_ft_p3457_4_resource_pressure_soak.sh`;
it proves remote execution and artifact shape, but it deliberately leaves
200-pane/high-scale claims as `skipped_not_proven` unless the host has at least
64 logical CPUs and 256 GiB memory and a live cockpit artifact is retained.
The retained v1 conformance summary at
`tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260513T172634Z/summary.json`
proves the schema/runtime lane at `remote_reduced` only. During memory
incidents, also preserve `domains.rss_residency`, `domains.storage_io`,
`domains.action_receipts`, `residency_buckets`, `action_receipts`, and
`artifact_paths` from the live cockpit output.

---

## Flow 3c: context horizon forecast

Use this before assigning more work to a pane that may be near context pressure,
after compaction/rate-limit detections, before large fanout changes, or when a
handoff decision needs evidence. The horizon is read-only forecast evidence; it
does not compact, hand off, pause, or mutate anything by itself.

1) Capture the machine contract:

```bash
ft robot --format json context horizon --horizon-window-ms 900000 \
  > /tmp/ft-context-horizon.json
ft robot --format toon context horizon --horizon-window-ms 900000 \
  > /tmp/ft-context-horizon.toon
ft doctor --json > /tmp/ft-doctor-context-horizon.json
```

2) Read the root state first:

| Field | Operator check |
| --- | --- |
| `generated_at_ms` | Forecast time; stale saved JSON is not live dispatch evidence. |
| `horizon_window_ms` | Lookahead window used by risk scoring. |
| `evidence_state` | `measured`/`inferred` may guide decisions; `stale`, `unavailable`, and `mixed` require domain review. |
| `unavailable_domains` | Missing or stale evidence that must be named before acting. |
| `artifact_paths` | Retained fixtures or proof artifacts used to audit the claim. |
| `raw_context_content_stored` | Must be `false`; any true value is a source regression. |

3) Interpret pane tiers conservatively:

| Tier | Normal next move |
| --- | --- |
| `green` | No context-specific action; keep observing. |
| `yellow` | Reduce fanout or prepare handoff material if citations support it. |
| `red` | Prefer handoff preparation, dry-run rotation planning, or assignment pause. |
| `black` | Stop assigning new work to that pane and collect an incident bundle if the cause is unclear. |
| `unknown` | Treat as missing evidence, not as healthy. |

4) Treat recommendations as dry-run advice. In v1 every recommendation must
have `mutation_allowed=false`. A `suggested_command` can be copied into a later
approval-gated workflow only after the operator separately checks policy,
ownership, and current pane state. Do not run Agent Mail repair/restart, RCH
service changes, destructive git/filesystem operations, or pane mutations just
because a context-horizon row exists.

5) Classify failures before filing or closing work:

| Class | Meaning |
| --- | --- |
| `source_regression` | Schema, fields, serialization, or recommendation semantics are wrong. |
| `privacy_violation` | Raw prompt, transcript, secret-like text, or unsafe artifact paths escaped. |
| `environment_blocked` | Storage, RCH, worker, or platform dependency prevented proof. |
| `unavailable_evidence` | The horizon ran but required evidence was missing. |
| `target_hardware_skipped` | High-scale hardware claims were not proven. |

6) Preserve proof accurately. For docs/schema changes, the focused truth gate is:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-r920m-6-docs-smoke \
  cargo test -p frankenterm --test docs_smoke \
  context_horizon_contract_docs_truth_gate -- --nocapture
```

For implementation changes, keep the core and robot lanes separate and record
the exact RCH command, selected worker, remote Cargo/test reachability, target
directory, artifacts, and failure class. RCH sync chatter alone is not proof.

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

Apply changes, then restart the watcher if you need the updated rules live immediately:

```bash
ft config validate
ft stop
ft watch
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
