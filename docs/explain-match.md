# Explain-Match Traces (Debugging Rule Detections)

Explain-match traces help you understand why a rule matched (or did not match)
without leaking secrets. Use them to debug false positives, false negatives, and
rule drift.

## When to use explain-match

- A rule fires when it should not (false positive).
- A rule does not fire when it should (false negative).
- You are validating a new rule or rule pack.
- You need to prove a detection is safe to act on.

## How to run

Human CLI (quick sanity checks):

```bash
wa rules list
wa rules test "Usage limit"  # shows matches, no trace
```

Robot CLI (machine-readable + trace):

```bash
wa robot rules list
wa robot rules test "Usage limit" --trace
```

Notes:
- `wa robot rules test --trace` currently returns a minimal trace
  (`anchors_checked`, `regex_matched`). Full trace output is planned.
- `--pack` is accepted for robot rules test, but filtering is not yet implemented.

## Mental model (phases)

Think of explain-match as a sequence:

1. **Eligibility gates**: agent type, dedupe, and other rule-level gates.
2. **Anchor checks**: fast string matches to narrow candidates.
3. **Regex match + captures**: full regex evaluation and named captures.
4. **Evidence assembly**: excerpts, spans, and extracted fields (redacted).

## Trace fields (full format)

When full explain-match traces are enabled, each match emits a `MatchTrace` with
these fields:

- `pack_id`, `rule_id`, `extractor_id`, `matched_text`, `confidence`
- `eligible`: whether the rule passed eligibility gates
- `gates[]`: `{ gate, passed, reason }` for each gate
- `evidence[]`: items with `kind`, `label`, `span`, `excerpt`, `truncated`
- `bounds`: `{ max_evidence_items, max_excerpt_bytes, max_capture_bytes,
  evidence_total, evidence_truncated, truncated_fields }`

Interpretation tips:
- `span` uses byte offsets in the original input string.
- `evidence.kind` typically includes anchors, matches, and captures.
- `truncated=true` on evidence means the excerpt was clipped to bounds.
- `bounds.evidence_truncated=true` indicates evidence was dropped for size.

Default bounds (from `TraceOptions::default()`):
- `max_evidence_items`: 8
- `max_excerpt_bytes`: 160
- `max_capture_bytes`: 120

## Redaction and privacy

All excerpts and captured values are passed through the policy redactor. Secrets
are replaced with `[REDACTED]` (or `[REDACTED:pattern]` when debug markers are
enabled). Do not expect raw secrets to appear in traces.

## Incident bundles

`wa diag bundle` includes rule traces for recent events when available:

- `traces/rule_traces.json`

These traces include redacted `matched_text` and extracted fields to help debug
rule behavior safely and share artifacts without leaking secrets.

