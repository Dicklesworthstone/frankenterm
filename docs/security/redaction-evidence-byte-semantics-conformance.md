# Redaction Evidence Byte Semantics Conformance Matrix

Bead: `ft-khxlh`

Source design: `ft-wjjkp.2`

Target implementation: `ft-wjjkp.3`

Status: static pre-code conformance matrix. This document does not claim that
the Rust implementation conforms yet; it freezes the requirements and fixture
coverage that the implementation bead must satisfy.

## Scope

This matrix turns `docs/security/redaction-evidence-byte-semantics.md` into a
retained conformance contract for the replacement of ambiguous
`BytesRedactionEvidence { matches, bytes_replaced }` semantics.

The contract covers:

- explicit evidence fields and derived-only values
- original-byte accounting for lossy UTF-8 decode paths
- streaming accounting for pending decoded text
- merge, cold-tier, and mmap evidence propagation
- privacy constraints for evidence storage and attestations
- removal of the legacy `bytes_replaced` compatibility name

The machine-readable inventory is
`fixtures/redaction-evidence-byte-semantics/requirements.v1.json`; the verifier
is `fixtures/redaction-evidence-byte-semantics/verify-requirements.sh`.

## Conformance Accounting

| Area | MUST requirements | Fixture-covered | Runtime implementation status |
|---|---:|---:|---|
| Evidence fields | 7 | 7 | pending `ft-wjjkp.3` |
| Derived-only values | 1 | 1 | pending `ft-wjjkp.3` |
| Streaming accounting | 2 | 2 | pending `ft-wjjkp.3` |
| Merge behavior | 1 | 1 | pending `ft-wjjkp.3` |
| Cold-tier propagation | 1 | 1 | pending `ft-wjjkp.3` |
| Mmap append/header accounting | 1 | 1 | pending `ft-wjjkp.3` |
| Privacy and legacy-name guardrails | 2 | 2 | pending `ft-wjjkp.3` |

Static coverage is complete for the requirements listed here. Runtime
conformance remains pending until `ft-wjjkp.3` lands and `ft-wjjkp.4` records
RCH proof for the implementation lanes named in the source design.

## Requirement Matrix

| ID | Level | Requirement | Fixture coverage |
|---|---|---|---|
| `REQ-FIELD-001` | MUST | `replacement_count` is the number of non-overlapping redactor spans replaced. | `FIX-VALID-UTF8-ONE-REPLACEMENT`, `FIX-MMAP-APPEND-HEADER` |
| `REQ-FIELD-002` | MUST | `original_input_bytes` is the exact source byte count represented by the returned result. Streaming pending bytes count only when emitted or finished. | `FIX-VALID-UTF8-ONE-REPLACEMENT`, `FIX-STREAM-SECRET-SPLIT` |
| `REQ-FIELD-003` | MUST | `decoded_input_text_bytes` is the UTF-8 byte length of the lossy-decoded text scanned before redaction. | `FIX-INVALID-UTF8-NO-REPLACEMENT`, `FIX-INVALID-UTF8-ADJACENT-REPLACEMENT` |
| `REQ-FIELD-004` | MUST | `redacted_output_bytes` is the UTF-8 byte length of the emitted redacted bytes. | `FIX-VALID-UTF8-MARKER-LONGER`, `FIX-STREAM-SECRET-SPLIT` |
| `REQ-FIELD-005` | MUST | `secret_input_bytes_replaced` is the exact original source byte count covered by replacement spans. | `FIX-VALID-UTF8-ONE-REPLACEMENT`, `FIX-INVALID-UTF8-ADJACENT-REPLACEMENT` |
| `REQ-LOSSY-001` | MUST | `lossy_input_bytes` counts original bytes represented by lossy replacement characters. | `FIX-INVALID-UTF8-NO-REPLACEMENT`, `FIX-INVALID-UTF8-SPLIT-STREAM` |
| `REQ-LOSSY-002` | MUST | `lossy_replacement_count` counts inserted `U+FFFD` replacement characters before redaction. | `FIX-INVALID-UTF8-NO-REPLACEMENT`, `FIX-INVALID-UTF8-SPLIT-STREAM` |
| `REQ-DERIVED-001` | MUST | `decode_was_lossy`, `text_length_delta`, and `original_to_output_delta` stay derived and are not stored evidence fields. | `FIX-VALID-UTF8-MARKER-LONGER` |
| `REQ-STREAM-001` | MUST | Streaming redaction uses mapped decoded text with spans that preserve text ranges, original byte counts, and lossy state. | `FIX-INVALID-UTF8-SPLIT-STREAM`, `FIX-STREAM-SECRET-SPLIT` |
| `REQ-STREAM-002` | MUST | A streaming call that buffers a partial value and emits no bytes reports zero original and output bytes for that call. | `FIX-INVALID-UTF8-SPLIT-STREAM`, `FIX-STREAM-SECRET-SPLIT` |
| `REQ-MERGE-001` | MUST | Merged redaction results saturating-add every evidence count while preserving output concatenation semantics. | `FIX-MERGE-OVERFLOW-EMISSIONS` |
| `REQ-COLD-001` | MUST | Cold-tier `RedactionEvidence` mirrors and preserves every byte-evidence field. | `FIX-COLD-TIER-CONVERSION` |
| `REQ-MMAP-001` | MUST | Mmap append reports carry the new evidence schema, and header redaction accounting uses `replacement_count`. | `FIX-MMAP-APPEND-HEADER` |
| `REQ-PRIV-001` | MUST | Evidence and fixtures retain counts, booleans, labels, and requirement ids only; no raw bytes, output text, snippets, context, offsets, or hashes of secret material. | `FIX-VALID-UTF8-ONE-REPLACEMENT`, `FIX-LEGACY-FIELD-ABSENT` |
| `REQ-COMPAT-001` | MUST | The legacy `bytes_replaced` field name does not survive as a compatibility alias in first-party evidence structs. | `FIX-LEGACY-FIELD-ABSENT` |

## Required Fixture Cases

| Fixture ID | Required edge case | Primary assertion |
|---|---|---|
| `FIX-VALID-UTF8-ONE-REPLACEMENT` | valid UTF-8 with one replacement | field counts distinguish input, output, and replaced source bytes |
| `FIX-VALID-UTF8-MARKER-LONGER` | valid UTF-8 with marker longer than replaced text | output growth is represented by explicit fields and derived deltas, not saturation |
| `FIX-INVALID-UTF8-NO-REPLACEMENT` | invalid UTF-8 with no replacement | lossy decode accounting is visible even when no redaction occurs |
| `FIX-INVALID-UTF8-ADJACENT-REPLACEMENT` | invalid UTF-8 adjacent to a replacement | lossy bytes and replaced source bytes are counted independently |
| `FIX-INVALID-UTF8-SPLIT-STREAM` | invalid UTF-8 split across streaming chunk boundaries | pending decoded spans count bytes exactly once when emitted |
| `FIX-STREAM-SECRET-SPLIT` | replacement target split across streaming chunk boundaries | no-emission calls report zero bytes and later emission owns the full accounting |
| `FIX-MERGE-OVERFLOW-EMISSIONS` | multiple forced-overflow emissions merged into one result | merge uses saturating addition for every evidence count |
| `FIX-COLD-TIER-CONVERSION` | cold-tier conversion preserving evidence fields | `From<BytesRedactionEvidence>` preserves every explicit count |
| `FIX-MMAP-APPEND-HEADER` | mmap append report and header accounting | append report carries the schema and header increments from `replacement_count` |
| `FIX-LEGACY-FIELD-ABSENT` | legacy evidence-name retirement | `bytes_replaced` is forbidden as a first-party evidence field or alias |

## Static Verification

Run:

```bash
bash fixtures/redaction-evidence-byte-semantics/verify-requirements.sh
jq empty fixtures/redaction-evidence-byte-semantics/requirements.v1.json
git diff --check -- docs/security/redaction-evidence-byte-semantics-conformance.md \
  fixtures/redaction-evidence-byte-semantics/requirements.v1.json \
  fixtures/redaction-evidence-byte-semantics/verify-requirements.sh
br dep cycles --json
```

These checks verify the retained requirements, fixture coverage, and privacy
guardrails. They are intentionally static. The implementation proof remains
the RCH-backed test/check lane described in
`docs/security/redaction-evidence-byte-semantics.md`.
