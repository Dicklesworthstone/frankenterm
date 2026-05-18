# Redaction Evidence Byte Semantics

Bead: `ft-wjjkp.2`

Status: design for the `ft-wjjkp.3` implementation slice.

## Current State

`BytesRedactionEvidence` currently lives in
`crates/frankenterm-core/src/redactor.rs` and has two fields:

```rust
pub struct BytesRedactionEvidence {
    pub matches: u32,
    pub bytes_replaced: u32,
}
```

`Redactor::redact_bytes_with_evidence` receives raw bytes, converts them with
`String::from_utf8_lossy`, detects secrets in that lossy text, redacts the
lossy text, and computes `bytes_replaced` as:

```rust
lossy.len().saturating_sub(redacted.len())
```

`StreamingRedactor::redact_chunk` uses the same lossy text semantics after
appending the chunk to its pending buffer. `merge_redaction_results` then sums
`matches` and `bytes_replaced`.

The cold-tier typed-state pipeline mirrors the same two-field shape in
`scrollback_cold_tier_pipeline::RedactionEvidence`, via
`impl From<BytesRedactionEvidence> for RedactionEvidence`. The mmap scrollback
writer returns `BytesRedactionEvidence` in `MmapAppendReport`, and the mmap
header increments its `redactions_applied` counter from `evidence.matches`.

## Ambiguity

The current `bytes_replaced` field is not an original input byte count. It is a
lossy-decoded UTF-8 text length delta.

That makes it honest for pure UTF-8 input, where `lossy.len() == bytes.len()`.
It is ambiguous for binary or mixed-encoding pane output because every invalid
UTF-8 byte sequence is converted to `U+FFFD`, which is three UTF-8 bytes in the
decoded text. The field can therefore overstate or understate the relationship
between the original bytes, the bytes that matched a secret, and the bytes
emitted after redaction.

The current field also saturates at zero when the replacement marker is longer
than the matched secret. That hides output growth. It is useful as a coarse
"some text got shorter" signal, but it is not a durable evidence schema.

## Proposed Schema

Replace the old two-field evidence with an explicit byte-accounting schema. Do
not keep a compatibility shim for `bytes_replaced`; the old name is the source
of the ambiguity.

```rust
pub struct BytesRedactionEvidence {
    pub replacement_count: u32,
    pub original_input_bytes: u64,
    pub decoded_input_text_bytes: u64,
    pub redacted_output_bytes: u64,
    pub secret_input_bytes_replaced: u64,
    pub lossy_input_bytes: u64,
    pub lossy_replacement_count: u32,
}
```

Field semantics:

| Field | Meaning |
|---|---|
| `replacement_count` | Number of non-overlapping secret spans replaced by the redactor. This is the old `matches` count with a name tied to replacement behavior. |
| `original_input_bytes` | Exact number of original source bytes represented by this returned redaction result. For streaming output, bytes retained in `pending` are counted when a later result emits or flushes them, not when the chunk first arrives. |
| `decoded_input_text_bytes` | UTF-8 byte length of the lossy-decoded text that was scanned for this result before redaction. This preserves the old measurement under an explicit name. |
| `redacted_output_bytes` | UTF-8 byte length of the emitted redacted output bytes. This is `result.bytes.len()` after redaction. |
| `secret_input_bytes_replaced` | Exact original source bytes covered by redactor match spans. This is the durable replacement-byte metric operators wanted from `bytes_replaced`. |
| `lossy_input_bytes` | Original input bytes that were represented by lossy replacement characters in the scanned text. Zero means the result was decoded without loss. |
| `lossy_replacement_count` | Number of `U+FFFD` replacement characters inserted into the scanned text before redaction. |

Derived values should stay derived:

- `decode_was_lossy = lossy_replacement_count > 0`
- `text_length_delta = decoded_input_text_bytes as i128 - redacted_output_bytes as i128`
- `original_to_output_delta = original_input_bytes as i128 - redacted_output_bytes as i128`

Keeping these as derived values prevents another stored field from drifting.

## Streaming Accounting

The streaming redactor cannot compute honest original-byte evidence from a
plain `String` pending buffer. `ft-wjjkp.3` should replace the pending buffer
with mapped decoded text:

```rust
struct PendingDecodedText {
    text: String,
    spans: Vec<DecodedSpan>,
}

struct DecodedSpan {
    text_start: usize,
    text_end: usize,
    original_bytes: u64,
    lossy: bool,
}
```

The decoder must produce the same text as `String::from_utf8_lossy` while also
recording how many original bytes each decoded text span represents. Redactor
match ranges are text byte ranges, so evidence can translate each matched span
back to exact original bytes by intersecting it with the span map.

`StreamingRedactor::redact_chunk` should report evidence for the prefix it
emits, not for every byte appended to `pending`. If a call buffers a partial
secret and emits no bytes, its returned evidence should have
`original_input_bytes == 0` and `redacted_output_bytes == 0`. The retained bytes
must be counted exactly once when emitted by a later chunk or by `finish`.

## Affected Surfaces

### Redactor Core

Update `crates/frankenterm-core/src/redactor.rs`:

- Replace `matches` with `replacement_count`.
- Replace `bytes_replaced` with the explicit byte fields above.
- Add the mapped lossy decode helper.
- Update `redact_bytes_with_evidence`, `StreamingRedactor::redact_chunk`,
  `StreamingRedactor::finish`, and `merge_redaction_results`.
- Keep `redactor_applied()` and `made_changes()` semantics, with
  `made_changes()` keyed to `replacement_count > 0`.

### Cold Tier

Update `crates/frankenterm-core/src/scrollback_cold_tier_pipeline.rs`:

- Mirror the new fields in `RedactionEvidence`.
- Update `impl From<BytesRedactionEvidence> for RedactionEvidence`.
- Update typed-state tests that currently construct
  `RedactionEvidence { matches, bytes_replaced }`.
- Keep `PipelineHealth::record_write` as an applied/not-applied privacy
  invariant; it should not infer application from replacement counts.

Update `docs/security/scrollback-cold-tier-pipeline.md` so it names the
evidence-bearing transitions and the exact `PipelineHealth::is_safe`
invariant.

### Mmap Scrollback

Update `crates/frankenterm-core/src/scrollback_mmap_writer.rs`:

- `MmapAppendReport.redaction` should carry the new evidence schema.
- `ScrollbackHeader.redactions_applied` currently increments from
  `evidence.matches`; use `replacement_count`.
- Tests that inspect append reports must assert both redaction output and
  byte-accounting fields.

### Storage

SQLite storage does not currently persist `BytesRedactionEvidence` or
`RedactionEvidence` directly. The storage risk is indirect:

- Cold-tier and mmap writers persist redacted bytes.
- Storage health currently records raw input and written byte totals through
  `PipelineHealth::record_write`, not secret-byte evidence.
- Existing pane segment storage and mmap mirror privacy tests must continue to
  prove raw secret text is absent.

No database migration is required unless `ft-wjjkp.3` chooses to persist the
new evidence fields. If it does, the migration must store counts only and must
not store raw spans, raw bytes, hashes of secrets, or surrounding context.

### Telemetry

The following counters remain aggregate health signals rather than byte
evidence:

- `PipelineHealth::redactions_applied_total`
- `PipelineHealth::chunks_written_without_redactor`
- `StructuredLogRow::ChunkWrite.redaction_applied`
- `ScrollbackHeader.redactions_applied`

Telemetry that wants byte semantics must consume the new explicit fields. It
must not reuse the old `bytes_replaced` name.

### Docs And Attestations

Existing redactor recall and read-path documents are not byte-evidence
schemas:

- `docs/security/redactor-coverage-methodology.md`
- `docs/security/redactor-coverage.json`
- `docs/security/read-path-redaction-matrix.md`

They should remain focused on detection coverage and outbound leak prevention.

The cold-tier and mmap evidence contract should be documented here and in
`docs/security/scrollback-cold-tier-pipeline.md`. Any future release
attestation for this evidence must include only source paths, test commands,
schema version, and aggregate counts. It must not include sample pane content,
matched secret text, raw binary payloads, or content hashes derived from secret
material.

## Privacy Constraints

The evidence schema may store counts and booleans only. It must not store:

- raw input bytes
- redacted output text
- matched secret snippets
- prefix or suffix context
- byte offsets that identify where a secret appeared in a user pane
- hashes or fingerprints of raw secret material

The mapped decoder is an in-memory implementation detail. It exists only long
enough to translate redactor match spans to byte counts.

## Migration Checklist For `ft-wjjkp.3`

1. Add the mapped lossy decoder and tests that prove it emits the same text as
   `String::from_utf8_lossy`.
2. Replace `BytesRedactionEvidence` fields with the proposed schema.
3. Update non-streaming byte redaction to report exact original bytes, decoded
   text bytes, output bytes, lossy counts, and secret input bytes replaced.
4. Replace `StreamingRedactor.pending: String` with mapped pending text so
   delayed emissions count original bytes exactly once.
5. Update `merge_redaction_results` to saturating-add every count and preserve
   output concatenation.
6. Update cold-tier `RedactionEvidence` and the `From<BytesRedactionEvidence>`
   conversion.
7. Update mmap append reports and header accounting to use
   `replacement_count`.
8. Update tests in `redactor.rs`, `scrollback_cold_tier_pipeline.rs`,
   `scrollback_mmap_writer.rs`, and `cold_tier_privacy_integration.rs`.
9. Remove or rename every first-party `bytes_replaced` evidence use. The name
   must not survive as a compatibility alias.
10. Update docs after the code shape lands.

Required edge tests:

- valid UTF-8 with one secret
- valid UTF-8 with a marker longer than the secret
- invalid UTF-8 with no secret
- invalid UTF-8 adjacent to a secret
- invalid UTF-8 split across streaming chunk boundaries
- a secret split across streaming chunk boundaries
- multiple forced-overflow emissions merged into one result
- cold-tier conversion preserving every evidence field
- mmap append report and header accounting using replacement counts

## RCH Proof Requirements For `ft-wjjkp.4`

Compiled proof must run through RCH, not local Cargo. The implementation bead
should retain logs for these lanes:

```bash
rch exec -- cargo test -p frankenterm-core --lib redactor::
rch exec -- cargo test -p frankenterm-core --lib scrollback_cold_tier_pipeline::
rch exec -- cargo test -p frankenterm-core --lib scrollback_mmap_writer::
rch exec -- cargo test -p frankenterm-core --test cold_tier_privacy_integration
rch exec -- cargo check -p frankenterm-core --lib
```

Static proof before the RCH lanes:

```bash
rg -n "bytes_replaced" crates/frankenterm-core/src crates/frankenterm-core/tests docs
git diff --check -- crates/frankenterm-core/src/redactor.rs \
  crates/frankenterm-core/src/scrollback_cold_tier_pipeline.rs \
  crates/frankenterm-core/src/scrollback_mmap_writer.rs \
  crates/frankenterm-core/tests/cold_tier_privacy_integration.rs \
  docs/security/redaction-evidence-byte-semantics.md \
  docs/security/scrollback-cold-tier-pipeline.md
```

Any remaining `bytes_replaced` hit must either be this design document
describing the retired field or an intentional external compatibility note
approved by the operator. First-party evidence structs should not expose it.
