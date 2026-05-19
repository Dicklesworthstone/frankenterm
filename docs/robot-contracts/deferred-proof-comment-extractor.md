# Deferred Proof Comment Extractor Contract

`ft.deferred_proof_comment_extraction.v1` is the static contract for the
Beads/comment extractor that feeds `ft.deferred_proof_receipt.v1`. The extractor
turns structured closeout footers and static-proof notes into deterministic
JSONL records. It must not treat vague prose as proof.

The schema is `docs/json-schema/ft-deferred-proof-comment-extraction.json`; the
retained fixture corpus lives under `fixtures/deferred-proof-replay/extractor/`.

## Required Semantics

- Every output line is one deterministic JSON object with a source digest,
  extraction state, reason codes, and either a receipt projection or `null`.
- Source provenance is mandatory: Beads comment id, timestamp, author, and
  SHA-256 digest of the exact source text.
- Raw pane text is forbidden. Fixtures and records may include only structured
  closeout text or redacted static-proof notes.
- Material Cargo commands must parse as real argv with
  `RCH_REQUIRE_REMOTE=1`, `RCH_NO_SELF_HEALING=1`, and
  `rch --no-self-healing exec --`.
- Static verifier notes may emit `static-verifier-v1` receipts only when the
  structured footer says material Cargo proof is not required.
- Duplicate comments are ineligible by `(bead_id, source_text_sha256)`.
- Ambiguous prose, missing commands, stale command shapes, local fallback
  evidence, and empty owned paths fail closed with actionable reason codes.
- Mixed static/RCH closeouts may preserve static-clean evidence but must still
  emit `wait_rch` when material remote proof remains blocked.

## Extraction States

| State | Meaning |
| --- | --- |
| `receipt_emitted` | A deterministic receipt projection was emitted. |
| `ineligible` | The source was parseable enough to reject with reason codes. |
| `duplicate` | The source duplicates an earlier `(bead_id, digest)` record. |

## Golden Corpus

The static verifier freezes source comments and expected JSONL records for:

- `remote-rch-blocked-closeout`
- `static-only-closeout`
- `mixed-static-rch-closeout`
- `ambiguous-prose-ineligible`
- `stale-command-ineligible`
- `duplicate-comment-ineligible`

It also rejects malformed fixture fragments for missing source text, raw pane
text storage, and unknown expected states.

Run:

```bash
bash tests/e2e/test_deferred_proof_comment_extractor_contract.sh
```

This verifier is static. Future Rust or Robot/MCP implementation proof must run
through remote-required RCH.
