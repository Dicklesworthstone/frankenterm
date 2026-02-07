# Incident Bundles and `wa reproduce`

Incident bundles are self-contained directories that capture enough context
to diagnose a problem without access to the original machine. They are
designed to be **safe to share** by default — secrets are redacted, output
is bounded, and a privacy budget limits total data volume.

## When to Generate a Bundle

Generate a bundle when:

- wa crashes and you need to report the issue
- A policy decision seems wrong and you want to reproduce it
- Rule matching behaves unexpectedly
- A workflow fails and you need to trace the execution
- You want to share diagnostic context with another operator or upstream

## Quick Start

```bash
# Export the latest crash as an incident bundle
wa reproduce export --kind crash

# Export a manual bundle (no crash required)
wa reproduce export --kind manual

# Replay a bundle to validate its contents
wa reproduce replay /path/to/wa_incident_crash_20260206_183000/ --mode policy
```

## Bundle Layout

Each bundle is a directory following the naming convention
`wa_incident_{kind}_{YYYYMMDD_HHMMSS}`:

```
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
| `incident_manifest.json` | Root metadata: kind, wa version, format version, file list, privacy budget |
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

After a crash, wa writes a crash report to the crash directory. Export it:

```bash
wa reproduce export --kind crash
```

This finds the latest crash report and packages it with health data,
config, and recent events into a single directory.

### Manual bundles

For non-crash diagnostics (unexpected policy decisions, rule misbehavior):

```bash
wa reproduce export --kind manual
```

This captures the same supporting data (health, config, events) without
crash-specific files.

### Output location

By default, bundles are written to the crash directory. Override with
`--out`:

```bash
wa reproduce export --kind manual --out /tmp/bundle
```

### JSON output

Add `--format json` for machine-readable output:

```bash
wa reproduce export --kind crash --format json
```

## Replaying Bundles

Replay validates a bundle's contents and checks for consistency. Three
replay modes are available, each with a defined set of checks.

### Policy mode

Validates crash/incident consistency and redaction correctness:

```bash
wa reproduce replay /path/to/bundle --mode policy
```

**Checks run:**
1. `manifest_valid` — manifest parses correctly
2. `version_compatible` — format version is readable by this wa version
3. `redaction_report_valid` — redaction report is well-formed
4. `no_secrets_leaked` — no secret patterns detected in any file
5. `crash_report_valid` — crash report parses (if present)
6. `db_metadata_valid` — DB metadata parses (if present)
7. `files_complete` — all manifest-listed files exist on disk

Use this mode for general bundle validation and before sharing externally.

### Rules mode

Validates event data structure and text boundaries:

```bash
wa reproduce replay /path/to/bundle --mode rules
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
wa reproduce replay /path/to/bundle --mode workflow
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
wa reproduce replay /path/to/bundle --mode policy
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

This allows bundles to be shared across wa versions within the same major
release.

## Examples

### Example 1: Watcher crash

```bash
# wa crashes during capture — crash report is auto-written
# Export the crash bundle
$ wa reproduce export --kind crash
wa reproduce export - Incident bundle exported

  Kind:     crash
  Path:     /home/user/.local/share/wa/crashes/wa_incident_crash_20260206_183000
  Files:    incident_manifest.json, README.md, redaction_report.json,
            crash_report.json, crash_manifest.json, health_snapshot.json,
            config_summary.toml

  Next steps:
  1. Review the bundle for sensitive data
  2. Share the bundle directory for analysis
  3. Run 'wa reproduce replay <path>' to replay

# Validate before sharing
$ wa reproduce replay ~/.local/share/wa/crashes/wa_incident_crash_20260206_183000 --mode policy
```

### Example 2: Unexpected policy denial

```bash
# A send command was denied but shouldn't have been
$ wa reproduce export --kind manual
# Replay to check policy consistency
$ wa reproduce replay /path/to/bundle --mode policy
```

### Example 3: Rule matching issue

```bash
# A rule didn't fire when expected
$ wa reproduce export --kind manual
# Replay to validate event structure
$ wa reproduce replay /path/to/bundle --mode rules
```

### Example 4: Workflow failure

```bash
# A workflow timed out mid-execution
$ wa reproduce export --kind manual
# Replay to check step timing and logs
$ wa reproduce replay /path/to/bundle --mode workflow
```

## Sharing Bundles

### Before sharing

1. Run `wa reproduce replay --mode policy` to verify redaction
2. Review `redaction_report.json` to confirm secrets were caught
3. Check that the privacy budget tier matches your sharing context
   (use `strict` for external vendors)

### Attaching to a GitHub issue

```bash
# Create a tarball
tar czf wa_incident.tar.gz wa_incident_crash_20260206_183000/

# Attach to the issue or share via a file hosting service
```

### Internal sharing

For internal debugging, the `verbose` tier provides more data. Adjust the
budget by passing options to the export:

```bash
wa reproduce export --kind manual --events 200
```

## Diagnostic Bundles

Separate from incident bundles, wa also provides a general diagnostic
bundle for health reporting:

```bash
wa diag bundle                        # generate diagnostic bundle
wa diag bundle --output /tmp/diag     # write to specific directory
wa diag bundle --force                # overwrite existing output
wa diag bundle --events 200           # include more recent events
```

Diagnostic bundles capture similar data (health, config, events, storage
stats) but are not tied to a specific incident. Use them for general
health checks and capacity planning.

## Programmatic Access

### Rust client

```rust
use wa_core::crash::{IncidentBundleOptions, IncidentKind, collect_incident_bundle};

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
wa robot reproduce export --kind crash --format json
```

Returns a JSON response envelope with the bundle path and file list.

## Troubleshooting

### "Bundle directory not found"

The replay command requires a path to an existing bundle directory:

```bash
# Wrong — file path
wa reproduce replay /path/to/incident_manifest.json

# Right — directory path
wa reproduce replay /path/to/wa_incident_crash_20260206_183000/
```

### "Incompatible bundle format"

The bundle was created with a different major version of wa. Upgrade or
downgrade wa to match the bundle's format version (shown in the manifest).

### "No crash bundles found"

No crash report exists in the crash directory. If wa crashed but no report
was written, the panic hook may not have been installed (happens only in
early startup failures).

### Redaction missed a secret

Report the pattern to improve detection. The redactor uses the same
patterns as `wa secrets scan`. Verify with:

```bash
wa reproduce replay /path/to/bundle --mode policy
```

The `no_secrets_leaked` check will flag any remaining patterns.
