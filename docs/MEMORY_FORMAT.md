# Audit-Marker Memory Format

**Purpose:** specify the YAML frontmatter shape that audit-sweep markers in
agent memory MUST carry so the `audit_marker_doctor.sh` helper can compute
falsifiable STALE/FRESH verdicts.

**Status:** load-bearing convention.
**Origin:** `docs/proposals/audit-marker-staleness.md` (ft-gkqej).
**Implementations:**

* Spec — this file (ft-l01pf).
* Backfill of existing markers — ft-hph8i.
* Doctor helper — `scripts/audit_marker_doctor.sh` (ft-nedq3).
* Skill-template emission — `multi-pass-bug-hunting` and
  `mock-code-finder` SKILL.md files updated under ft-cqhfe.

## Why this format exists

Per-agent memory accumulates "subsystem cleared, don't re-sweep" markers.
Without anchoring them to a git SHA + path glob, those markers rot
silently: the 2026-04-20 sweep markers described `patterns.rs` /
`storage.rs` / `frankenterm/mux+term+codec` as clean while their swept
paths quietly accumulated 24 / 154 / 91 commits over five days.
Bare-prose markers can't tell the next agent "this audit is no longer
trustworthy."

The fix is to make the marker carry enough metadata for an automated
doctor to ask "how many commits have landed on these paths since the
sweep ran?" and refuse to skip re-sweeping when the answer exceeds a
per-subsystem threshold.

## Required frontmatter

Every audit-sweep marker MUST start with:

```yaml
---
name: <subsystem>-bug-sweep-YYYY-MM-DD
description: <one-line summary including stale-after threshold>
type: project
swept_at_sha: <full git SHA at sweep time>
sweep_paths:
  - <path/being/swept/1>
  - <path/being/swept/2>
stale_after_commits: <integer>
findings_summary: <cat-A / cat-B / cat-C tallies + headline notes>
---
```

The frontmatter must end with the closing `---` line; everything after
that is free-form prose describing the sweep methodology, fix beads
filed, etc.

### Field reference

| Field                  | Type            | Required | Notes |
|------------------------|-----------------|----------|-------|
| `name`                 | string          | yes      | Stable identifier; used as the marker's primary key. Convention: `<subsystem>-<type>-sweep-YYYY-MM-DD` (e.g. `patterns-subsystem-bug-sweep-2026-04-20`). |
| `description`          | string          | yes      | One-line summary used by indices like `MEMORY.md`. SHOULD include the stale-after threshold inline so a reader scanning the index sees the contract without opening the file. |
| `type`                 | string          | yes      | Always `project` for sweep markers. Other memory types (`feedback`, `reference`, `user`) don't use this format. |
| `swept_at_sha`         | git SHA (full)  | yes      | `git rev-parse HEAD` captured BEFORE the sweep starts. Subsequent fix commits must NOT shift this anchor; they're the commits the doctor counts. |
| `sweep_paths`          | YAML list       | yes      | Resolved paths (not globs) the sweep actually touched. Order doesn't matter; the doctor passes them all to `git rev-list --count <sha>..HEAD -- <paths…>`. |
| `stale_after_commits`  | integer         | yes      | Per-subsystem churn threshold. See "Threshold guidance" below. |
| `findings_summary`     | string          | yes      | Condensed cat-A / cat-B / cat-C tallies + headline notes. Should mirror what's in `description` but with more detail. |
| `originSessionId`      | UUID            | optional | Carried by markers written through the auto-memory system; preserve it on backfill but don't require it for hand-written markers. |

### Threshold guidance

Pick `stale_after_commits` by churn class:

| Subsystem class                                          | Threshold | Examples |
|----------------------------------------------------------|-----------|----------|
| Storage / runtime / hot-path (>100 commits / 30 days)    | **30**    | `storage.rs`, `runtime_async.rs`, `recorder_*.rs` |
| Mux / codec / vendored crates (medium churn)             | **20**    | `frankenterm/mux`, `frankenterm/codec`, `frankenterm/term` |
| Patterns / config / DSL / detection rules (low churn)    | **5**     | `patterns.rs`, `pattern_trigger.rs`, `config/*.rs` |

The thresholds are calibrated so a marker stays FRESH for roughly 2
weeks of normal development on the swept paths. Picking too high makes
the doctor toothless; picking too low produces false STALE alarms that
trigger unneeded re-sweeps. When unsure, use the next-tier-up value
(prefer too eager over too forgiving).

## Example marker

```yaml
---
name: patterns-subsystem-bug-sweep-2026-04-20
description: Multi-pass bug sweep of patterns.rs + pattern_trigger.rs found 3 B-class hardening candidates, 0 cat-A defects. Stale-after 5 commits to the swept paths — see audit_marker_doctor (ft-nedq3).
type: project
originSessionId: 5522ffd1-efb3-498d-ac65-39cf84ec1bc4
swept_at_sha: 2ca20e1887d37a0b6ddcf3115c7ead95415b7daa
sweep_paths:
  - crates/frankenterm-core/src/patterns.rs
  - crates/frankenterm-core/src/pattern_trigger.rs
stale_after_commits: 5
findings_summary: 0 cat-A, 3 cat-B candidates documented
---
Multi-pass bug hunt on `crates/frankenterm-core/src/patterns.rs` (5686 lines)
+ `pattern_trigger.rs` (608 lines) on 2026-04-20. …
```

(This is the exact shape ft-hph8i backfilled onto the three existing
sweep markers in `~/.claude/projects/-Users-jemanuel-projects-frankenterm/memory/`.)

## Validation

Run the doctor against a memory directory:

```bash
scripts/audit_marker_doctor.sh                         # default ~/.claude/.../memory
scripts/audit_marker_doctor.sh --dir /path/to/memory   # override
scripts/audit_marker_doctor.sh --json                  # machine-readable
```

Exit codes:

| Code | Meaning |
|------|---------|
| `0`  | Every marker is FRESH. |
| `1`  | At least one marker is STALE. Suitable for `if ! audit_marker_doctor; then …` CI gates. |
| `2`  | No SHA-anchored markers found in the directory (probably the wrong `--dir`). |

The doctor walks every `*.md` with a top-level `swept_at_sha:` line, so
it picks up new markers that emit this format without any allowlist
edits. Markers that don't emit the format are invisible to it; they
won't fail validation but they also won't benefit from the staleness
check.

## Authoring new markers

The two skills that emit sweep markers — `multi-pass-bug-hunting` and
`mock-code-finder` — were updated under ft-cqhfe to include an
"Audit Marker Output" section instructing the agent running the
sweep to:

1. Capture `git rev-parse HEAD` BEFORE Pass 1 / Phase 1.
2. Record the resolved paths handed to `ubs` / `rg` / `ast-grep`.
3. Pick the threshold per the table above.
4. Emit the marker with the frontmatter shown in this file.

If you're writing a sweep marker by hand (not via one of those skills),
follow the same checklist. The doctor doesn't care HOW the marker was
produced — only whether the frontmatter shape is right.

## Updating existing markers

When a sweep is refreshed (re-run after the doctor flagged STALE),
update in place:

* `swept_at_sha` — the new HEAD.
* `findings_summary` — the new tallies.
* `description` — keep the stale-after wording so the index entry
  reads consistently.
* `sweep_paths` — only if scope changed.
* `stale_after_commits` — only if churn class changed.

Don't create a parallel `<name>-2026-MM-DD` file for a re-sweep of the
same subsystem; the doctor matches on the file, not the date in the
name. Multiple sweep dates accumulate noise without adding signal.

## Out of scope

* Markers of other types (`feedback`, `reference`, `user`) — those
  don't carry sweep metadata and are not consumed by the doctor.
* The richer "guard report" object model that `runtime_compat_surface_guard`
  used to maintain — that was migration-era scaffolding retired under
  ft-yqd3w / ft-3hi74. Sweep markers are a leaner replacement that lives
  in agent memory rather than in compiled code.

## Cross-reference table

| Concern                                              | Where it lives |
|------------------------------------------------------|----------------|
| Why this format exists (proposal + audit numbers)    | `docs/proposals/audit-marker-staleness.md` (ft-gkqej) |
| Backfilled examples                                  | `~/.claude/projects/-Users-jemanuel-projects-frankenterm/memory/project_*sweep*.md` (ft-hph8i) |
| Validator                                            | `scripts/audit_marker_doctor.sh` (ft-nedq3) |
| Authoring instructions for skill agents              | `multi-pass-bug-hunting/SKILL.md` + `mock-code-finder/SKILL.md` "Audit Marker Output" sections (ft-cqhfe) |
| This spec                                            | `docs/MEMORY_FORMAT.md` (ft-l01pf — this file) |
