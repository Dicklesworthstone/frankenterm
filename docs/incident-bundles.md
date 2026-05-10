# Incident Bundles and `ft reproduce`

Status: current replay-bundle contract plus `ft-krsq0` swarm-ops extension
contract. The replay-bundle files below are implemented today; the swarm
operator extension is the contract that downstream `ft-krsq0.*` beads must
converge on before claiming implementation.

Incident bundles are self-contained directories that capture enough context
to diagnose a problem without access to the original machine. They are
designed to be **safe to share** by default — secrets are redacted, output
is bounded, and a privacy budget limits total data volume.

## When to Generate a Bundle

Generate a bundle when:

- ft crashes and you need to report the issue
- A policy decision seems wrong and you want to reproduce it
- Rule matching behaves unexpectedly
- A workflow fails and you need to trace the execution
- You want to share diagnostic context with another operator or upstream

## Quick Start

```bash
# Export the latest crash as an incident bundle
ft reproduce export --kind crash

# Export a manual bundle (no crash required)
ft reproduce export --kind manual

# Replay a bundle to validate its contents
ft reproduce replay /path/to/wa_incident_crash_20260206_183000/ --mode policy
```

## Bundle Layout

Each bundle is a directory following the naming convention currently emitted by
the Rust collector:

```text
wa_incident_{kind}_{YYYYMMDD_HHMMSS}
```

Older and future-facing docs may mention `ft_incident_*`, but the live
`bundle_dirname` helper and `collect_incident_bundle` path still write
`wa_incident_*`. Until the producer is renamed, closeout evidence must cite the
actual path emitted by the command under test.

```text
wa_incident_crash_20260206_183000/
├── incident_manifest.json   # versioned metadata (always present)
├── README.md                # human-readable instructions (always present)
├── redaction_report.json    # what was redacted — counts only, no secrets
├── crash_report.json        # panic info (crash bundles only)
├── crash_manifest.json      # crash-time metadata (crash bundles only)
├── health_snapshot.json     # last HealthSnapshot (if available)
├── config_summary.toml      # redacted configuration (if provided)
├── db_metadata.json         # schema version + storage stats (if DB available)
└── recent_events.json       # bounded event summaries (if DB + events exist)
```

### Required files

Every valid bundle contains at least:

| File | Purpose |
|------|---------|
| `incident_manifest.json` | Root metadata: kind, ft version, format version, file list, privacy budget |
| `README.md` | Human-readable overview with replay instructions |
| `redaction_report.json` | Counts of redactions applied (never contains secrets) |

### Crash-only files

These appear only when `--kind crash` is used:

| File | Purpose |
|------|---------|
| `crash_report.json` | Panic message, backtrace (truncated to budget), thread info |
| `crash_manifest.json` | Crash-time metadata: timestamp, signal, exit code |

### Optional files

Present when the relevant data source is available:

| File | Purpose |
|------|---------|
| `health_snapshot.json` | Runtime health at bundle time: queue depths, backpressure tier, scheduler state |
| `config_summary.toml` | Active configuration with secrets replaced by `[REDACTED]` |
| `db_metadata.json` | Schema version, table row counts, storage statistics |
| `recent_events.json` | Most recent events (bounded by privacy budget) |

## Exporting Bundles

### Crash bundles

After a crash, ft writes a crash report to the crash directory. Export it:

```bash
ft reproduce export --kind crash
```

This finds the latest crash report and packages it with health data,
config, and recent events into a single directory.

### Manual bundles

For non-crash diagnostics (unexpected policy decisions, rule misbehavior):

```bash
ft reproduce export --kind manual
```

This captures the same supporting data (health, config, events) without
crash-specific files.

### Output location

By default, bundles are written to the crash directory. Override with
`--out`:

```bash
ft reproduce export --kind manual --out /tmp/bundle
```

### JSON output

Add `--format json` for machine-readable output:

```bash
ft reproduce export --kind crash --format json
```

## Replaying Bundles

Replay validates a bundle's contents and checks for consistency. Three
replay modes are available, each with a defined set of checks.

### Policy mode

Validates crash/incident consistency and redaction correctness:

```bash
ft reproduce replay /path/to/bundle --mode policy
```

**Checks run:**
1. `manifest_valid` — manifest parses correctly
2. `version_compatible` — format version is readable by this ft version
3. `redaction_report_valid` — redaction report is well-formed
4. `no_secrets_leaked` — no secret patterns detected in any file
5. `crash_report_valid` — crash report parses (if present)
6. `db_metadata_valid` — DB metadata parses (if present)
7. `files_complete` — all manifest-listed files exist on disk

Use this mode for general bundle validation and before sharing externally.

### Rules mode

Validates event data structure and text boundaries:

```bash
ft reproduce replay /path/to/bundle --mode rules
```

**Checks run:**
1. `manifest_valid`
2. `version_compatible`
3. `redaction_report_valid`
4. `no_secrets_leaked`
5. `events_structure_valid` — events have required fields
6. `events_text_bounded` — all text excerpts are within budget limits
7. `files_complete`

Use this when investigating rule or pattern matching issues.

### Workflow mode

Validates workflow step logs and execution traces:

```bash
ft reproduce replay /path/to/bundle --mode workflow
```

**Checks run:**
1. `manifest_valid`
2. `version_compatible`
3. `redaction_report_valid`
4. `no_secrets_leaked`
5. `workflow_steps_valid` — step logs have required fields
6. `workflow_timing_valid` — step timestamps are monotonic
7. `workflow_no_raw_output` — step output is within bounds
8. `files_complete`

Use this when investigating workflow failures or timing issues.

## Privacy Budget

Every bundle is produced under a privacy budget that bounds total data
volume and controls what is included. Three tiers are available:

| Tier | Max total | Max per file | Events | Excerpt length | Use case |
|------|-----------|--------------|--------|----------------|----------|
| **strict** | 256 KiB | 64 KiB | excluded | 100 chars | Sharing with external vendors |
| **default** | 1 MiB | 256 KiB | 50 most recent | 200 chars | Standard bug reports |
| **verbose** | 4 MiB | 1 MiB | 200 most recent | 500 chars | Internal deep debugging |

The default tier is used unless overridden. The budget controls:

- **max_bytes_per_file** — individual files are truncated with a marker if
  they exceed this limit
- **max_total_bytes** — the entire bundle stops adding files once this
  limit is reached
- **max_lines_per_log** — log/text files are line-limited
- **max_output_excerpt_len** — event text previews are character-limited
- **max_backtrace_len** — crash backtraces are truncated
- **include_recent_events** — whether `recent_events.json` is generated
- **max_recent_events** — how many events to include

The applied budget is recorded in `incident_manifest.json` under the
`privacy_budget` field so reviewers know what limits were in effect.

## Swarm Operator Extension Contract (`ft-krsq0`)

The current incident bundle is useful for crash, rule, policy, and workflow
replay. The `ft-krsq0` extension defines the next operator-facing shape for
black-box swarm triage: a read-only, redacted, portable capture of live
coordination, proof, resource, and process state when a massive-agent fleet is
already misbehaving.

This extension does not replace the replay-bundle contract above. It adds
source-level provenance and degradation semantics so another agent can inspect a
bundle on a different machine and know exactly what was collected, skipped,
unavailable, stale, or redacted.

### Non-Mutating Collection Rules

Collectors for this extension must be read-only by default:

- do not send text to panes or otherwise interact with active terminal state
- do not claim, reopen, close, or reassign Beads
- do not run `am doctor repair`, `am doctor fix`, service restarts, or process
  kills for Agent Mail or shared daemons
- do not run Cargo, RCH proof lanes, benchmarks, or expensive tests while
  collecting the bundle
- do not mutate git state; collect dirty-tree status as evidence only
- do not attach debuggers or sample processes unless the privacy tier and
  command-line flags explicitly allow the bounded platform sampler

If a source cannot be collected without violating these rules, the collector
must emit a structured warning and continue with the remaining sources.

### Extension Layout

The extension may be a standalone bundle or a subdirectory inside a current
incident bundle. The current `collect_incident_bundle` implementation preserves
the existing `wa_incident_{kind}_{timestamp}` bundle shape and embeds this
contract under `incident_manifest.json` as the `swarm` object. Standalone future
collectors may promote the same fields to the manifest top level. The required
layout is:

```text
swarm_incident_{kind}_{YYYYMMDDTHHMMSSZ}/
├── incident_manifest.json
├── README.md
├── redaction_report.json
├── warnings.jsonl
├── sources/
│   ├── robot_state.json
│   ├── pane_text_summaries.json
│   ├── tailer_capture_health.json
│   ├── resource_pressure_cockpit.json
│   ├── proof_rch_evidence.json
│   ├── beads_coordination_snapshot.json
│   ├── git_dirty_tree.json
│   └── process_sample.json
└── provenance/
    └── source_commands.json
```

Only `incident_manifest.json`, `README.md`, `redaction_report.json`, and
`warnings.jsonl` are mandatory. Every file under `sources/` is optional and
must have a matching manifest entry with an explicit source status.

### Manifest Fields

The extension manifest must include these fields. In the current in-tree
collector, read these under `incident_manifest.json` → `swarm` unless the field
already exists on the legacy top-level incident manifest.

| Field | Meaning |
| --- | --- |
| `contract_id` | Stable string, `ft.swarm_incident_bundle.v1`. |
| `format_version` | Bundle format version. Compatible extensions should use the current major and increment the minor version. |
| `bundle_id` | Unique id derived from kind, UTC timestamp, and optional operator-supplied label. |
| `kind` | `crash`, `manual`, `swarm_degraded`, `resource_pressure`, `proof_failure`, or `coordination_failure`. |
| `created_at` | UTC ISO-8601 timestamp from the collecting host. |
| `generator` | ft version, git commit if known, hostname class, OS, and command/API surface. |
| `privacy_budget` | Applied tier and hard limits. Must match the table above unless `tier=custom`. |
| `collection_policy` | Read-only guarantees, allowed optional samplers, timeout limits, and whether live pane text was permitted. |
| `environment` | Secret-safe runtime summary such as OS/architecture and whether cwd lookup succeeded; never raw environment variables or local cwd paths. |
| `sources` | Array of per-source entries described below. |
| `warnings` | Array of structured warning records also written to `warnings.jsonl`; source entries refer to these by `warning_ids`. |
| `redaction_summary` | Counts only; no raw secret values. |
| `total_size_bytes` | Total bytes written after truncation/redaction. |

Each `sources[]` entry must include:

| Field | Meaning |
| --- | --- |
| `name` | Stable source name, for example `robot_state` or `beads_coordination_snapshot`. |
| `file` | Relative path when collected; omitted when unavailable or skipped. |
| `status` | `collected`, `skipped`, `unavailable`, `failed`, or `stale`. |
| `evidence_state` | `measured`, `simulated`, `unavailable`, `stale`, or `mixed`. |
| `source_surface` | Rust API, robot command, Beads command, git command, or platform tool used. |
| `mutates_state` | Must be `false` for default collection. |
| `generated_at` | Source timestamp, nullable only for unavailable sources. |
| `freshness_ms` | Age at bundle creation. |
| `max_age_ms` | Freshness budget for that source. |
| `redaction` | `none`, `partial`, `full`, or `not_applicable`. |
| `privacy_tier` | Tier applied to that source. |
| `size_bytes` | Bytes written for the source payload, zero when absent. |
| `warning_ids` | Warnings explaining partial, stale, unavailable, or failed collection. |

### Source Inventory

| Source | Default collection | Required safety behavior |
| --- | --- | --- |
| `robot_state` | `ft robot state` or internal equivalent without pane writes. | Include pane ids, titles, domains, cwd where already exposed, state, and timestamps. Do not include full text. |
| `pane_text_summaries` | Bounded `ft robot get-text --tail` summaries only when privacy tier permits. | Redact and truncate; use placeholders for sensitive or excluded panes. |
| `tailer_capture_health` | Runtime/tailer/capture health snapshots and lag counters. | Report unavailable fields explicitly instead of synthesizing green health. |
| `resource_pressure_cockpit` | Current resource cockpit snapshot if the producer is wired. | Preserve `measured`, `simulated`, `unavailable`, and `stale` states from the cockpit contract. |
| `proof_rch_evidence` | Paths and verdict summaries for existing proof/RCH artifacts. | Do not run new proof commands. Do not treat RCH sync, queue, or transfer logs as proof. |
| `beads_coordination_snapshot` | `br ready`, `br show`, `bv --robot-*`, or fallback swarm snapshot summaries. | Do not claim or reopen work; include active assignees and stale-bead recommendations as evidence. |
| `git_dirty_tree` | `git status --short` and optional `git diff --stat`. | Never stage, revert, reset, clean, or delete files. |
| `process_sample` | Optional bounded OS/process summary. | Off by default for strict/default sharing; when enabled, use timeouts and never kill or restart processes. |
| `agent_mail` | Health/inbox/list status if the API is available. | On DB/API failure, retry once and record `unavailable`; do not repair or restart Agent Mail. |

### Excluded Data

Default bundles must not include:

- unredacted config, environment variables, credentials, access tokens, or
  Agent Mail message bodies
- full pane scrollback or full terminal transcripts
- raw process memory, core dumps, heap dumps, or debugger dumps
- private source files unrelated to the active incident
- new heavy Cargo/RCH logs produced during bundle collection
- unbounded stdout/stderr, database rows, search index contents, or attachment
  payloads

An operator may opt in to a more privileged source only when the command line
and manifest both record the elevated privacy tier and the reason.

### Degraded-Source Example

This fixture shape is intentionally small and deterministic so future golden
tests can validate schema, warning, redaction, and provenance behavior without
using live private pane text:

```json
{
  "contract_id": "ft.swarm_incident_bundle.v1",
  "format_version": {"major": 1, "minor": 1},
  "bundle_id": "swarm_incident_coordination_failure_20260510T111240Z",
  "kind": "coordination_failure",
  "created_at": "2026-05-10T11:12:40Z",
  "generator": {
    "ft_version": "0.1.0-test",
    "git_commit": "13b31ede6",
    "surface": "planned ft reproduce export --kind manual --swarm"
  },
  "privacy_budget": {
    "tier": "default",
    "max_total_bytes": 1048576,
    "max_bytes_per_file": 262144,
    "max_output_excerpt_len": 200
  },
  "collection_policy": {
    "mutating_actions_allowed": false,
    "pane_text_allowed": "summaries_only",
    "process_sampler": "disabled",
    "agent_mail_repair_allowed": false
  },
  "sources": [
    {
      "name": "robot_state",
      "file": "sources/robot_state.json",
      "status": "collected",
      "evidence_state": "measured",
      "source_surface": "ft robot state",
      "mutates_state": false,
      "generated_at": "2026-05-10T11:12:39Z",
      "freshness_ms": 1000,
      "max_age_ms": 30000,
      "redaction": "not_applicable",
      "privacy_tier": "default",
      "size_bytes": 913,
      "warning_ids": []
    },
    {
      "name": "pane_text_summaries",
      "file": "sources/pane_text_summaries.json",
      "status": "collected",
      "evidence_state": "measured",
      "source_surface": "ft robot get-text --tail 40",
      "mutates_state": false,
      "generated_at": "2026-05-10T11:12:39Z",
      "freshness_ms": 1000,
      "max_age_ms": 30000,
      "redaction": "partial",
      "privacy_tier": "default",
      "size_bytes": 512,
      "warning_ids": ["pane_text.redacted"]
    },
    {
      "name": "agent_mail",
      "status": "unavailable",
      "evidence_state": "unavailable",
      "source_surface": "MCP Agent Mail fetch_inbox/list_agents",
      "mutates_state": false,
      "generated_at": null,
      "freshness_ms": null,
      "max_age_ms": 30000,
      "redaction": "not_applicable",
      "privacy_tier": "default",
      "size_bytes": 0,
      "warning_ids": ["agent_mail.database_error"]
    }
  ],
  "warnings": [
    {
      "id": "pane_text.redacted",
      "severity": "info",
      "message": "Pane text contains scrubbed placeholders such as [REDACTED] and [PANE_TEXT_TRUNCATED]."
    },
    {
      "id": "agent_mail.database_error",
      "severity": "warning",
      "message": "Agent Mail was unavailable after the allowed retry; collector did not repair or restart it."
    }
  ],
  "redaction_summary": {
    "total_redactions": 2,
    "files_with_redactions": 1
  },
  "total_size_bytes": 4096
}
```

A matching `sources/pane_text_summaries.json` fixture should use scrubbed
placeholders rather than raw transcript text:

```json
[
  {
    "pane_id": 7,
    "title": "codex",
    "tail_lines": 40,
    "raw_text_included": false,
    "redacted_excerpt": "build failed after [REDACTED] ... [PANE_TEXT_TRUNCATED]",
    "redaction": "partial"
  }
]
```

### Validation Targets

Downstream implementation beads must provide fixtures and checks that fail on:

- raw secret patterns in any bundle file
- missing required manifest fields or source entries
- source payloads present without matching manifest provenance
- unavailable or failed sources that lack warning ids
- nondeterministic timestamps in committed fixtures
- privacy-budget truncation that writes more bytes than the configured limit
- claims that a source was measured when it was simulated, stale, or unavailable

If validation invokes Cargo, clippy, benchmarks, or E2E harnesses, the proof
must run through RCH and preserve the exact command plus artifact path in the
closing evidence.

## Redaction

All bundle files pass through the secret redactor before being written.
The redactor detects patterns like API keys, tokens, credentials, and
connection strings, replacing them with `[REDACTED]` markers.

The `redaction_report.json` file records:
- Total number of redaction replacements
- Number of files that had at least one redaction

It never contains the secrets themselves.

### Verify redaction

The policy replay mode includes a `no_secrets_leaked` check that re-scans
all bundle files for known secret patterns. Run it before sharing:

```bash
ft reproduce replay /path/to/bundle --mode policy
```

If the check fails, the bundle should not be shared until the leak is
investigated.

## Format Versioning

Bundles include a `format_version` field (currently `1.0`) in the
manifest. Replay tooling uses this to determine compatibility:

- **Same major version** — fully compatible
- **Newer minor version** — compatible but some fields may be missing in
  older readers (warning issued)
- **Different major version** — incompatible, replay refuses to proceed

This allows bundles to be shared across ft versions within the same major
release.

## Examples

### Example 1: Watcher crash

```bash
# ft crashes during capture — crash report is auto-written
# Export the crash bundle
$ ft reproduce export --kind crash
ft reproduce export - Incident bundle exported

  Kind:     crash
  Path:     /home/user/.local/share/ft/crashes/wa_incident_crash_20260206_183000
  Files:    incident_manifest.json, README.md, redaction_report.json,
            crash_report.json, crash_manifest.json, health_snapshot.json,
            config_summary.toml

  Next steps:
  1. Review the bundle for sensitive data
  2. Share the bundle directory for analysis
  3. Run 'ft reproduce replay <path>' to replay

# Validate before sharing
$ ft reproduce replay ~/.local/share/ft/crashes/wa_incident_crash_20260206_183000 --mode policy
```

### Example 2: Unexpected policy denial

```bash
# A send command was denied but shouldn't have been
$ ft reproduce export --kind manual
# Replay to check policy consistency
$ ft reproduce replay /path/to/bundle --mode policy
```

### Example 3: Rule matching issue

```bash
# A rule didn't fire when expected
$ ft reproduce export --kind manual
# Replay to validate event structure
$ ft reproduce replay /path/to/bundle --mode rules
```

### Example 4: Workflow failure

```bash
# A workflow timed out mid-execution
$ ft reproduce export --kind manual
# Replay to check step timing and logs
$ ft reproduce replay /path/to/bundle --mode workflow
```

## Sharing Bundles

### Before sharing

1. Run `ft reproduce replay --mode policy` to verify redaction
2. Review `redaction_report.json` to confirm secrets were caught
3. Check that the privacy budget tier matches your sharing context
   (use `strict` for external vendors)

### Attaching to a GitHub issue

```bash
# Create a tarball
tar czf incident_bundle.tar.gz wa_incident_crash_20260206_183000/

# Attach to the issue or share via a file hosting service
```

### Internal sharing

For internal debugging, the `verbose` tier provides more data. Adjust the
budget by passing options to the export:

```bash
ft reproduce export --kind manual --events 200
```

## Diagnostic Bundles

Separate from incident bundles, ft also provides a general diagnostic
bundle for health reporting:

```bash
ft diag bundle                        # generate diagnostic bundle
ft diag bundle --output /tmp/diag     # write to specific directory
ft diag bundle --force                # overwrite existing output
ft diag bundle --events 200           # include more recent events
```

Diagnostic bundles capture similar data (health, config, events, storage
stats) but are not tied to a specific incident. Use them for general
health checks and capacity planning.

## Programmatic Access

### Rust client

```rust
use frankenterm_core::crash::{collect_incident_bundle, IncidentBundleOptions, IncidentKind};

let opts = IncidentBundleOptions {
    crash_dir: &layout.crash_dir,
    config_path: Some(&config_path),
    out_dir: &output_dir,
    kind: IncidentKind::Manual,
    db_path: Some(&db_path),
};

let result = collect_incident_bundle(&opts)?;
println!("Bundle at: {}", result.path.display());
println!("Files: {:?}", result.files);
```

### Robot mode

```bash
ft robot reproduce export --kind crash --format json
```

Returns a JSON response envelope with the bundle path and file list.

## Troubleshooting

### "Bundle directory not found"

The replay command requires a path to an existing bundle directory:

```bash
# Wrong — file path
ft reproduce replay /path/to/incident_manifest.json

# Right — directory path
ft reproduce replay /path/to/wa_incident_crash_20260206_183000/
```

### "Incompatible bundle format"

The bundle was created with a different major version of ft. Upgrade or
downgrade ft to match the bundle's format version (shown in the manifest).

### "No crash bundles found"

No crash report exists in the crash directory. If ft crashed but no report
was written, the panic hook may not have been installed (happens only in
early startup failures).

### Redaction missed a secret

Report the pattern to improve detection. The redactor uses the same
patterns as `ft secrets scan`. Verify with:

```bash
ft reproduce replay /path/to/bundle --mode policy
```

The `no_secrets_leaked` check will flag any remaining patterns.
