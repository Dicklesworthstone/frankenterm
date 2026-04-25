# Proposal: Anchor Audit Markers to Git SHAs

**Status:** Draft
**Tracking bead:** ft-gkqej ([STRATEGIC/LOW])
**Author:** strategic-analysis pane (cod), 2026-04-25
**Scope:** Per-agent `MEMORY.md` indexes + audit-finding write-up files

## Problem

Per-agent memory indexes accumulate "subsystem-cleared" markers like:

  - patterns-subsystem-bug-sweep-2026-04-20: 0 cat-A, 3 cat-B candidates
    documented — *don't re-sweep unless scope changes*
  - capture-storage-subsystem-bug-sweep-2026-04-20: storage.rs (34k
    lines, no capture.rs) + recorder_* cluster; 0 cat-A
  - mux-term-codec-mock-finder-sweep-2026-04-20: 0 production stubs

Each marker tells a future agent (often a future *me*) "this subsystem
was clean five days ago, you can skip it." Under low-concurrency
single-agent operation that's a fine perf hint. Under the actual
operating regime — 50+ named agent identities (`MEMORY.md` lists
~85 in the active roster) committing concurrently — the markers
silently mis-promise: between when the sweep was run and when the next
agent reads the marker, the underlying file may have been heavily
edited.

Empirical proof, measured 2026-04-25 against the 2026-04-20 markers:

  | Subsystem (sweep-2026-04-20)         | Commits since |
  | ------------------------------------ | ------------- |
  | patterns.rs                          | 19 |
  | storage.rs                           | 25 |
  | mux + term + codec                   | 39 |
  | recorder_*.rs                        |  5 |
  | TOTAL across the three "cleared" sweeps | **88** |

88 commits in 5 days against subsystems that are flagged "don't
re-sweep". Even if 90% of those are unrelated rustfmt / type-rename
churn, the failure mode is that an agent skips re-checking the
subsystem because the marker says it was clean — and a regression
introduced in commit N+1 escapes scrutiny.

The cost of a wrong "don't re-sweep" is not symmetric with the cost of
a redundant sweep. A stale marker silently de-prioritises checking a
subsystem with potentially fresh bugs; a redundant sweep just spends a
little token budget.

## Design

Two changes, both small and additive.

### 1. Anchor every audit marker to a git SHA + path glob

Today's marker:

    [patterns-subsystem-bug-sweep-2026-04-20] patterns.rs+pattern_trigger.rs
    cleanly hardened; 0 cat-A, 3 cat-B candidates documented — don't
    re-sweep unless scope changes

Proposed marker:

    [patterns-subsystem-bug-sweep] sweep@<sha-7> over <path-glob>
    (cat-A: 0, cat-B: 3 candidates documented). Stale-after:
    <N> commits to <path-glob>. Last verified <date>.

Concretely, every audit-finding memory file gains a frontmatter block:

    ---
    sweep_subject: patterns
    sweep_paths:
      - crates/frankenterm-core/src/patterns.rs
      - crates/frankenterm-core/src/pattern_trigger.rs
    swept_at_sha: 23e0ec94
    stale_after_commits: 5
    findings_summary: 0 cat-A, 3 cat-B candidates
    ---

The `MEMORY.md` index entry collapses to a one-liner pointing at the
file; the structured staleness data lives next to the actual write-up.

### 2. Trivial helper that flags stale markers

A 30-line script (`scripts/audit_marker_doctor.sh` or equivalent) that:

1. Walks every `project_*sweep*.md` (and similar) for the frontmatter.
2. For each, runs `git rev-list --count <swept_at_sha>..HEAD -- <paths>`.
3. Prints `STALE` when the count exceeds `stale_after_commits`.
4. Optional `--mark-stale` flag rewrites the index entry to
   `[STALE] [marker-name]` so the next agent reading `MEMORY.md` sees
   the warning inline.

CI doesn't need to gate on this; it's a discoverability tool. An agent
running the script before deciding "skip this subsystem" gets a
one-screen dashboard of which markers are still trustworthy.

## Why not "just delete the markers"

Tempting but wrong. The findings themselves (`cat-A: 0, cat-B: 3
candidates documented`) are real signal — they tell future agents what
the *previous* sweep already considered, so a re-sweep can focus on
deltas instead of starting cold. We want to keep the findings. We just
want to retire the *recommendation* ("don't re-sweep") and replace it
with a falsifiable staleness predicate.

## Out of scope

* Changing how individual sweeps are run (skill: `multi-pass-bug-hunting`,
  `mock-code-finder`). Those produce the findings; this proposal
  governs how the findings are stored and aged.
* Replacing the per-agent memory directory with a shared store. Big
  separate decision; this proposal is purely additive.

## Child beads (proposed)

To be filed under ft-gkqej once this proposal lands:

1. **Backfill SHA + paths frontmatter on existing sweep markers.**
   Walk the three current sweep files (`project_patterns_sweep_findings.md`,
   `project_capture_storage_sweep_findings.md`, `project_mux_term_codec_sweep.md`),
   add the `swept_at_sha` / `sweep_paths` / `stale_after_commits`
   frontmatter pulled from `git log --before=2026-04-21`.
2. **Write `audit_marker_doctor` helper.** ~30 lines of bash/python
   that flags stale markers per the spec above. Agnostic to which
   agent's memory is being inspected.
3. **Update sweep-skill templates.** `multi-pass-bug-hunting` /
   `mock-code-finder` skills should emit the new frontmatter shape by
   default so future markers don't go in stale.
4. **Convention doc.** A short `MEMORY_FORMAT.md` describing the
   frontmatter spec so cross-agent memory remains parseable.
